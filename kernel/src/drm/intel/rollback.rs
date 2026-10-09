//! Boot-only ADL-N combo-HDMI transaction. Original implementation using i915
//! disable/enable dependency order, not reverse replay of status dwords.
//! Only one primary plane on pipe A is admitted; unsupported firmware topology
//! is refused before the first write. GT engines/forcewake are never programmed.
use alloc::{format, string::String, vec::Vec};

use spin::Mutex;

use super::{
    clk,
    firmware_scanout::FirmwareScanout,
    firmware_snapshot::Snapshot,
    gmbus::PollTimer,
    gtt::{Checkpoint, Gtt},
    hpd::Ddi,
    regs::{self, Meaning, Register, Registers, ddi as d, dpll as pll, pipe as p},
};
const ENABLE: u32 = 1 << 31;
const RUNNING: u32 = 1 << 30;
const LOCK: u32 = 1 << 30;
const POWER: u32 = 1 << 27;
const POWER_STATE: u32 = 1 << 26;
const PLLS: [Register; 2] = [pll::DPLL0_ENABLE, pll::DPLL1_ENABLE];
const OVERLAYS: [Register; 6] = [
    Register::read_only("FW_PLANE2_CTL_A", 0x70280, Meaning::BringUp, None),
    Register::read_only("FW_PLANE3_CTL_A", 0x70380, Meaning::BringUp, None),
    Register::read_only("FW_PLANE4_CTL_A", 0x70480, Meaning::BringUp, None),
    Register::read_only("FW_PLANE5_CTL_A", 0x70580, Meaning::BringUp, None),
    Register::read_only("FW_PLANE6_CTL_A", 0x70680, Meaning::BringUp, None),
    Register::read_only("FW_PLANE7_CTL_A", 0x70780, Meaning::BringUp, None),
];
const CURSOR: Register = Register::read_only("FW_CUR_CTL_A", 0x70080, Meaning::BringUp, None);
const EDP: Register = Register::read_only("FW_TRANSCONF_EDP", 0x7f008, Meaning::BringUp, None);
// Group TX_DW5 stores broadcast to all four lane shadows. Capture all of
// them, not just lane 0; offsets follow intel_combo_phy_regs.h's LN stride.
pub(crate) const LANE_DW5: [[Register; 4]; 2] = [
    [
        regs::port::PORT_TX_DW5_LN0_A,
        Register::read_write("PORT_TX_DW5_LN1(A)", 0x162994, Meaning::BringUp, None),
        Register::read_write("PORT_TX_DW5_LN2(A)", 0x162a94, Meaning::BringUp, None),
        Register::read_write("PORT_TX_DW5_LN3(A)", 0x162b94, Meaning::BringUp, None),
    ],
    [
        regs::port::PORT_TX_DW5_LN0_B,
        Register::read_write("PORT_TX_DW5_LN1(B)", 0x6c994, Meaning::BringUp, None),
        Register::read_write("PORT_TX_DW5_LN2(B)", 0x6ca94, Meaning::BringUp, None),
        Register::read_write("PORT_TX_DW5_LN3(B)", 0x6cb94, Meaning::BringUp, None),
    ],
];
const DW5_GROUPS: [Register; 2] = [regs::port::PORT_TX_DW5_GRP_A, regs::port::PORT_TX_DW5_GRP_B];
fn is_phy(r: Register) -> bool {
    r.name().starts_with("PORT_") || r.name().starts_with("ICL_PHY_MISC")
}
fn is_dw5(r: Register) -> bool {
    DW5_GROUPS.contains(&r) || LANE_DW5.iter().flatten().any(|lane| *lane == r)
}
struct ReadOnly<'a, R>(&'a R);
impl<R: Registers> Registers for ReadOnly<'_, R> {
    fn read(&self, r: Register) -> Option<u32> {
        self.0.read(r)
    }
    fn read64(&self, r: Register) -> Option<u64> {
        self.0.read64(r)
    }
    fn write(&self, _: Register, _: u32) -> bool {
        false
    }
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Data,
    Control,
    Power,
    Transient,
    Forbidden,
}
fn policy(register: Register) -> (Class, u32) {
    if !register.is_writable() || regs::table::INTERRUPT.contains(&register) {
        return (Class::Forbidden, 0);
    }
    for phy in regs::COMBO_PHYS {
        if [
            phy.comp_dw0,
            phy.comp_dw1,
            phy.comp_dw8,
            phy.comp_dw9,
            phy.comp_dw10,
            phy.phy_misc,
            phy.tx_dw8,
            phy.pcs_dw1,
        ]
        .contains(&register)
        {
            return (Class::Forbidden, 0);
        }
    }
    if [regs::CDCLK_CTL, regs::CDCLK_PLL_ENABLE].contains(&register) {
        // Generic MMIO writes cannot change CDCLK. The only supported mutation
        // is Transaction::transition_cdclk(), which pairs PCODE and records
        // an explicit reverse transition for rollback.
        return (Class::Forbidden, 0);
    }
    if [
        regs::GMBUS1,
        regs::GMBUS3,
        regs::aux::DP_AUX_CH_CTL_A,
        regs::aux::DP_AUX_CH_DATA0_A,
        regs::aux::DP_AUX_CH_DATA1_A,
        regs::aux::DP_AUX_CH_DATA2_A,
        regs::aux::DP_AUX_CH_DATA3_A,
        regs::aux::DP_AUX_CH_DATA4_A,
        regs::aux::DP_AUX_CH_CTL_B,
        regs::aux::DP_AUX_CH_DATA0_B,
        regs::aux::DP_AUX_CH_DATA1_B,
        regs::aux::DP_AUX_CH_DATA2_B,
        regs::aux::DP_AUX_CH_DATA3_B,
        regs::aux::DP_AUX_CH_DATA4_B,
        regs::aux::DP_AUX_CH_CTL_D,
        regs::aux::DP_AUX_CH_DATA0_D,
        regs::aux::DP_AUX_CH_DATA1_D,
        regs::aux::DP_AUX_CH_DATA2_D,
        regs::aux::DP_AUX_CH_DATA3_D,
        regs::aux::DP_AUX_CH_DATA4_D,
        regs::aux::DP_AUX_CH_CTL_E,
        regs::aux::DP_AUX_CH_DATA0_E,
        regs::aux::DP_AUX_CH_DATA1_E,
        regs::aux::DP_AUX_CH_DATA2_E,
        regs::aux::DP_AUX_CH_DATA3_E,
        regs::aux::DP_AUX_CH_DATA4_E,
    ]
    .contains(&register)
    {
        return (Class::Transient, u32::MAX);
    }
    if [
        regs::HSW_PWR_WELL_CTL2,
        regs::ICL_PWR_WELL_CTL_AUX2,
        regs::ICL_PWR_WELL_CTL_DDI2,
    ]
    .contains(&register)
    {
        return (Class::Power, 0xaaaa_aaaa);
    }
    if register == regs::DC_STATE_EN {
        return (Class::Power, super::power::DC_STATE_MASK);
    }
    if register.name().starts_with("DBUF_CTL_") {
        return (Class::Power, ENABLE);
    }
    if PLLS.contains(&register) {
        return (Class::Control, !(LOCK | POWER_STATE));
    }
    if [
        p::PIPECONF_A,
        p::PLANE_CTL_A,
        p::PLANE_SURF_A,
        d::TRANS_CLK_SEL_A,
        d::TRANS_DDI_FUNC_CTL_A,
        d::DDI_BUF_CTL_A,
        d::DDI_BUF_CTL_B,
        pll::ICL_DPCLKA_CFGCR0,
    ]
    .contains(&register)
    {
        let mask = if register == p::PIPECONF_A {
            !RUNNING
        } else if [d::DDI_BUF_CTL_A, d::DDI_BUF_CTL_B].contains(&register) {
            !(1 << 7)
        } else {
            u32::MAX
        };
        return (Class::Control, mask);
    }
    if register == regs::SHOTPLUG_CTL_DDI {
        return (Class::Data, 0x8888);
    }
    (Class::Data, u32::MAX)
}
pub(crate) fn snapshot_read32(device: &impl Registers, reg: Register) -> Option<u32> {
    if let Some(index) = DW5_GROUPS.iter().position(|group| *group == reg) {
        device.read(LANE_DW5[index][0])
    } else {
        device.read(reg)
    }
}
fn read(device: &impl Registers, reg: Register) -> Result<u32, String> {
    snapshot_read32(device, reg).ok_or_else(|| format!("{} unreadable", reg.name()))
}
fn masked_write(device: &impl Registers, reg: Register, value: u32) -> Result<(), String> {
    let (_, mask) = policy(reg);
    if mask == 0 {
        return Err(format!("{} has no restore policy", reg.name()));
    }
    let current = read(device, reg)?;
    let mut payload = (current & !mask) | (value & mask);
    // Never echo HPD W1C pulse latches. Status is not restorable configuration.
    if reg == regs::SHOTPLUG_CTL_DDI {
        payload &= !0x3333;
    }
    if [
        regs::HSW_PWR_WELL_CTL2,
        regs::ICL_PWR_WELL_CTL_AUX2,
        regs::ICL_PWR_WELL_CTL_DDI2,
    ]
    .contains(&reg)
    {
        payload &= 0xaaaa_aaaa;
    }
    if reg.name().starts_with("DBUF_CTL_") {
        payload &= !RUNNING;
    }
    // Other status fields are RO. Do not intentionally write their saved bits.
    if PLLS.contains(&reg) {
        payload &= !(LOCK | POWER_STATE);
    }
    if reg == p::PIPECONF_A {
        payload &= !RUNNING;
    }
    if [d::DDI_BUF_CTL_A, d::DDI_BUF_CTL_B].contains(&reg) {
        payload &= !(1 << 7);
    }
    if !device.write(reg, payload) {
        return Err(format!("{} write refused", reg.name()));
    }
    if let Some(index) = DW5_GROUPS.iter().position(|group| *group == reg) {
        for lane in LANE_DW5[index] {
            if read(device, lane)? & mask != value & mask {
                return Err(format!("{} broadcast shadow mismatch", lane.name()));
            }
        }
    }
    let actual = read(device, reg)?;
    if actual & mask != value & mask {
        return Err(format!("{} writable-field restore mismatch", reg.name()));
    }
    Ok(())
}
fn wait(
    device: &impl Registers,
    timer: &impl PollTimer,
    reg: Register,
    mask: u32,
    value: u32,
) -> Result<(), String> {
    let start = timer.now_micros();
    for _ in 0..200_000 {
        if read(device, reg)? & mask == value {
            return Ok(());
        }
        if timer.now_micros().saturating_sub(start) >= 100_000 {
            break;
        }
        timer.pause();
    }
    Err(format!(
        "{} status timeout mask={mask:#x} expected={value:#x}",
        reg.name()
    ))
}
fn plane_disable_latched(device: &impl Registers, timer: &impl PollTimer) -> Result<(), String> {
    if read(device, p::PIPECONF_A)? & RUNNING == 0 {
        return Ok(());
    }
    let start = timer.now_micros();
    let mut last = read(device, p::PIPEDSL_A)? & 0xfffff;
    let mut wraps = 0;
    let mut sample_at = start;
    for _ in 0..500_000 {
        let now = timer.now_micros();
        if now.saturating_sub(start) >= 200_000 {
            break;
        }
        if now >= sample_at {
            let line = read(device, p::PIPEDSL_A)? & 0xfffff;
            if line < last {
                wraps += 1;
            }
            if wraps >= 2 {
                return Ok(());
            }
            last = line;
            sample_at = now.saturating_add(200);
        }
        timer.pause();
    }
    Err(String::from(
        "plane disable did not pass two live frame boundaries",
    ))
}
struct Journal {
    touched: Vec<Register>,
    failed: bool,
    attempts: usize,
    fail_at: Option<usize>,
    clock_transitioned: bool,
}
pub(crate) struct Transaction<'a, R: Registers> {
    pub(crate) before: Snapshot,
    pub(crate) firmware: FirmwareScanout,
    pub(crate) ddi: Ddi,
    pub(crate) pll_id: u8,
    pub(crate) reusable_phys: Vec<super::phy::PhyState>,
    cdclk_before: clk::CdclkObservation,
    device: &'a R,
    journal: Mutex<Journal>,
}
impl<'a, R: Registers> Transaction<'a, R> {
    pub(crate) fn begin(device: &'a R, timer: &impl PollTimer) -> Result<Self, String> {
        let mut before = Snapshot::capture(device);
        let firmware = FirmwareScanout::capture(device)?;
        firmware.verify(device, timer)?;
        if !firmware.primary_a() {
            return Err(String::from(
                "rollback supports only a single firmware primary on pipe A",
            ));
        }
        for reg in OVERLAYS.into_iter().chain([CURSOR, EDP]) {
            let value = read(device, reg)?;
            let mask = if reg == CURSOR {
                0x3f
            } else {
                ENABLE | if reg == EDP { RUNNING } else { 0 }
            };
            if value & mask != 0 {
                return Err(format!(
                    "{} active: unsupported firmware topology",
                    reg.name()
                ));
            }
        }
        let func = firmware.value(d::TRANS_DDI_FUNC_CTL_A).unwrap();
        let port = (func >> 27) & 0xf;
        if (func >> 24) & 7 > 1 || !(1..=2).contains(&port) {
            return Err(String::from(
                "firmware is not combo HDMI/DVI on DDI A/B; DP/Type-C/eDP rollback is not guessed",
            ));
        }
        let ddi = if port == 1 { Ddi::A } else { Ddi::B };
        let (active, other) = if port == 1 {
            (d::DDI_BUF_CTL_A, d::DDI_BUF_CTL_B)
        } else {
            (d::DDI_BUF_CTL_B, d::DDI_BUF_CTL_A)
        };
        if read(device, active)? & (ENABLE | (1 << 7)) != ENABLE
            || read(device, other)? & ENABLE != 0
        {
            return Err(String::from(
                "firmware combo link is not exclusively enabled/non-idle",
            ));
        }

        let cdclk_before = clk::observe(device).map_err(|e| e.describe())?;
        if !cdclk_before.usable() {
            return Err(String::from(
                "firmware CDCLK not usable; firmware-preserving modeset requires a usable \
                 starting clock",
            ));
        }
        let index = ddi.index();
        let selected_pll = ((read(device, pll::ICL_DPCLKA_CFGCR0)? >> (index * 2)) & 3) as u8;
        if selected_pll > 1 {
            return Err(String::from("firmware uses non-combo PLL; no writes"));
        }
        let pll_state = read(device, PLLS[usize::from(selected_pll)])?;
        if pll_state & (ENABLE | LOCK | POWER | POWER_STATE) != ENABLE | LOCK | POWER | POWER_STATE
        {
            return Err(String::from(
                "firmware-selected PLL is not stably powered/enabled/locked",
            ));
        }
        let state = super::phy::init_one(
            &ReadOnly(device),
            regs::COMBO_PHYS[index as usize],
            index == 0,
        )
        .map_err(|e| {
            format!(
                "active firmware PHY calibration not reusable without writes: {}",
                e.describe()
            )
        })?;
        if !state.already_initialised {
            return Err(String::from("active PHY was not initialized by firmware"));
        }
        let mut reusable_phys = Vec::new();
        reusable_phys
            .try_reserve_exact(1)
            .map_err(|_| String::from("PHY state allocation failed"))?;
        reusable_phys.push(state);
        if read(device, regs::GMBUS2)? & (1 << 9) != 0 || read(device, regs::GMBUS4)? != 0 {
            return Err(String::from("firmware GMBUS active/interrupt-owned"));
        }
        for entry in &before.entries {
            if entry.register.is_writable()
                && policy(entry.register).0 != Class::Forbidden
                && entry.value.is_none()
            {
                return Err(format!("{} before-image absent", entry.register.name()));
            }
        }
        before.admitted = true;
        // Reserve all tracking capacity before any hardware mutation. A write
        // whose original value/policy is absent will fail closed at the wrapper.
        let mut touched = Vec::new();
        touched
            .try_reserve_exact(before.entries.len())
            .map_err(|_| String::from("rollback journal allocation failed"))?;
        Ok(Self {
            before,
            firmware,
            ddi,
            pll_id: selected_pll,
            reusable_phys,
            cdclk_before,
            device,
            journal: Mutex::new(Journal {
                touched,
                failed: false,
                attempts: 0,
                fail_at: None,
                clock_transitioned: false,
            }),
        })
    }
    pub(crate) fn pipes_disabled(&self) -> bool {
        let pipes_off = [p::PIPECONF_A, p::PIPECONF_B, p::PIPECONF_C, p::PIPECONF_D]
            .into_iter()
            .all(|register| {
                self.device
                    .read(register)
                    .is_some_and(|value| value & ENABLE == 0)
            });
        let link_off = [
            (d::TRANS_DDI_FUNC_CTL_A, ENABLE),
            (d::DDI_BUF_CTL_A, ENABLE),
            (d::DDI_BUF_CTL_B, ENABLE),
        ]
        .into_iter()
        .all(|(register, mask)| {
            self.device
                .read(register)
                .is_some_and(|value| value & mask == 0)
        });
        pipes_off && link_off
    }

    /// Change CDCLK only after the old firmware pipe/link has been quiesced.
    /// The transaction records the need to restore both CDCLK and PCODE's
    /// voltage request if any later modeset step fails.
    pub(crate) fn transition_cdclk(
        &self,
        timer: &impl PollTimer,
        target: clk::CdclkEntry,
    ) -> Result<clk::CdclkTransitionReport, String> {
        if !self.pipes_disabled() {
            return Err(String::from(
                "CDCLK transition requires all display pipes disabled",
            ));
        }
        let old = clk::observe(self.device).map_err(|e| e.describe())?;
        if !old.usable() {
            return Err(String::from("CDCLK transition requires usable old state"));
        }
        super::pcode::prepare_cdclk_change(self.device, timer)
            .map_err(|error| format!("CDCLK PCode PREPARE failed: {error:?}"))?;
        self.journal.lock().clock_transitioned = true;
        let transition = clk::transition(self.device, old, target, None, true)
            .map_err(|error| format!("CDCLK transition failed: {}", error.describe()))?;
        if !transition.after.usable() || transition.after.entry != Some(target) {
            return Err(format!(
                "CDCLK transition did not read back the requested table row: {} kHz",
                transition.after.cdclk_khz
            ));
        }
        super::pcode::commit_cdclk_voltage(self.device, timer, transition.after.cdclk_khz)
            .map_err(|error| format!("CDCLK PCode voltage update failed: {error:?}"))?;
        Ok(transition)
    }

    fn restore_cdclk(&self, timer: &impl PollTimer) -> Result<(), String> {
        if !self.pipes_disabled() {
            return Err(String::from(
                "CDCLK rollback requires all display pipes disabled",
            ));
        }
        let target = self
            .cdclk_before
            .entry
            .ok_or_else(|| String::from("original CDCLK had no source table row"))?;
        let old_pipe = match self.cdclk_before.pipe_field {
            pipe @ 0..=3 => Some(pipe as u8),
            7 => None,
            field => {
                return Err(format!(
                    "original CDCLK pipe field {field} cannot be restored safely"
                ));
            }
        };
        let old = clk::observe(self.device).map_err(|error| error.describe())?;
        super::pcode::prepare_cdclk_change(self.device, timer)
            .map_err(|error| format!("CDCLK rollback PCode PREPARE failed: {error:?}"))?;
        let transition = clk::transition(self.device, old, target, old_pipe, true)
            .map_err(|error| format!("CDCLK rollback transition failed: {}", error.describe()))?;
        if !transition.after.usable()
            || transition.after.entry != Some(target)
            || transition.after.cdclk_khz != self.cdclk_before.cdclk_khz
        {
            return Err(format!(
                "CDCLK rollback did not restore original {} kHz table row",
                self.cdclk_before.cdclk_khz
            ));
        }
        super::pcode::commit_cdclk_voltage(self.device, timer, transition.after.cdclk_khz)
            .map_err(|error| format!("CDCLK rollback PCode voltage update failed: {error:?}"))
    }
    /// Explicit opt-in hardware rollback exercise, after admission/capture.
    pub(crate) fn inject_failure(&self, attempt: usize) -> Result<(), String> {
        if attempt == 0 || attempt > 4096 {
            return Err(String::from("intel.modeset.fail_write must be 1..4096"));
        }
        self.journal.lock().fail_at = Some(attempt);
        Ok(())
    }
    pub(crate) fn attempts(&self) -> usize {
        self.journal.lock().attempts
    }
    fn saved(&self, reg: Register) -> Result<u32, String> {
        self.before
            .entries
            .iter()
            .find(|e| e.register == reg)
            .and_then(|e| e.value)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| format!("{} not captured", reg.name()))
    }
    fn write_checked(&self, reg: Register, value: u32) -> Result<(), String> {
        if self.write(reg, value) {
            Ok(())
        } else {
            Err(format!("{} transaction write refused", reg.name()))
        }
    }
    pub(crate) fn quiesce(&self, timer: &impl PollTimer) -> Result<(), String> {
        self.write_checked(p::PLANE_CTL_A, read(self, p::PLANE_CTL_A)? & !ENABLE)?;
        self.write_checked(p::PLANE_SURF_A, read(self, p::PLANE_SURF_A)?)?;
        plane_disable_latched(self, timer)?;
        self.disable_link(self, timer)?;
        Ok(())
    }
    fn disable_link(&self, device: &impl Registers, timer: &impl PollTimer) -> Result<(), String> {
        masked_write(
            device,
            p::PIPECONF_A,
            read(device, p::PIPECONF_A)? & !ENABLE,
        )?;
        wait(device, timer, p::PIPECONF_A, RUNNING, 0)?;
        masked_write(
            device,
            d::TRANS_DDI_FUNC_CTL_A,
            read(device, d::TRANS_DDI_FUNC_CTL_A)? & !ENABLE,
        )?;
        for reg in [d::DDI_BUF_CTL_A, d::DDI_BUF_CTL_B] {
            let current = read(device, reg)?;
            if (current | self.saved(reg)?) & ENABLE != 0 {
                masked_write(device, reg, current & !ENABLE)?;
                wait(device, timer, reg, 1 << 7, 1 << 7)?;
            }
        }
        masked_write(device, d::TRANS_CLK_SEL_A, 0)?;
        masked_write(
            device,
            pll::ICL_DPCLKA_CFGCR0,
            read(device, pll::ICL_DPCLKA_CFGCR0)? | (1 << (10 + self.ddi.index())),
        )?;
        let reg = PLLS[usize::from(self.pll_id)];
        masked_write(device, reg, read(device, reg)? & !ENABLE)?;
        wait(device, timer, reg, LOCK, 0)?;
        Ok(())
    }
    pub(crate) fn restore(
        &self,
        timer: &impl PollTimer,
        gtt: &Gtt,
        image: &Checkpoint,
    ) -> Result<String, String> {
        // Reverse the dependency stages, not status-dword replay. Bypass the
        // failed transaction wrapper; restoration must still attempt real I/O.
        masked_write(
            self.device,
            p::PLANE_CTL_A,
            read(self.device, p::PLANE_CTL_A)? & !ENABLE,
        )?;
        masked_write(
            self.device,
            p::PLANE_SURF_A,
            read(self.device, p::PLANE_SURF_A)?,
        )?;
        plane_disable_latched(self.device, timer)?;
        self.disable_link(self.device, timer)?;
        if self.journal.lock().clock_transitioned {
            self.restore_cdclk(timer)?;
        }
        // SAFETY: native plane/pipe/link are off; changed GGTT entries were
        // allocated only for our retained framebuffer, never submitted to GT.
        unsafe { gtt.restore_checkpoint(image) }.map_err(|e| e.describe())?;
        let journal = self.journal.lock();
        // GMBUS commands are transient, not restorable dwords. Cancel/reset
        // the channel, then restore its idle firmware selector/interrupt mask.
        if journal
            .touched
            .iter()
            .any(|reg| (regs::GMBUS0.offset()..=regs::GMBUS5.offset()).contains(&reg.offset()))
        {
            for (reg, value) in [
                (regs::GMBUS0, 0),
                (regs::GMBUS4, 0),
                (regs::GMBUS1, 1 << 31),
                (regs::GMBUS1, 0),
            ] {
                if !self.device.write(reg, value) {
                    return Err(String::from("GMBUS rollback reset refused"));
                }
            }
            wait(self.device, timer, regs::GMBUS2, 1 << 9, 0)?;
            masked_write(self.device, regs::GMBUS0, self.saved(regs::GMBUS0)?)?;
            masked_write(self.device, regs::GMBUS4, self.saved(regs::GMBUS4)?)?;
        }
        for &reg in journal.touched.iter().rev() {
            if policy(reg).0 == Class::Data && !is_phy(reg) {
                masked_write(self.device, reg, self.saved(reg)?)?;
            }
        }
        // Combo PLL divider data is restored above while PLLs are disabled.
        // Power must precede enable, and lock must precede the clock route.
        {
            let reg = PLLS[usize::from(self.pll_id)];
            let saved = self.saved(reg)?;
            masked_write(self.device, reg, saved & !ENABLE)?;
            wait(
                self.device,
                timer,
                reg,
                POWER_STATE,
                if saved & POWER != 0 { POWER_STATE } else { 0 },
            )?;
            if saved & ENABLE != 0 {
                masked_write(self.device, reg, saved)?;
                wait(self.device, timer, reg, LOCK, LOCK)?;
            }
        }
        masked_write(
            self.device,
            pll::ICL_DPCLKA_CFGCR0,
            self.saved(pll::ICL_DPCLKA_CFGCR0)?,
        )?;
        for (index, group) in DW5_GROUPS.iter().enumerate() {
            if journal.touched.contains(group) {
                masked_write(
                    self.device,
                    *group,
                    read(self.device, LANE_DW5[index][0])? & !ENABLE,
                )?;
            }
        }
        for &reg in journal.touched.iter().rev() {
            if policy(reg).0 == Class::Data && is_phy(reg) && !is_dw5(reg) {
                masked_write(self.device, reg, self.saved(reg)?)?;
            }
        }
        for (index, group) in DW5_GROUPS.iter().enumerate().rev() {
            if journal.touched.contains(group) {
                // Group write first, then lane-specific originals, so a lane-0
                // replay cannot destroy the other firmware lane settings.
                masked_write(self.device, *group, self.saved(*group)? & !ENABLE)?;
                for reg in LANE_DW5[index].into_iter().rev() {
                    masked_write(self.device, reg, self.saved(reg)?)?;
                }
            }
        }
        masked_write(
            self.device,
            d::TRANS_CLK_SEL_A,
            self.saved(d::TRANS_CLK_SEL_A)?,
        )?;
        masked_write(
            self.device,
            d::TRANS_DDI_FUNC_CTL_A,
            self.saved(d::TRANS_DDI_FUNC_CTL_A)?,
        )?;
        // HDMI encoder enable before transcoder, then plane arm (i915's
        // intel_encoders_enable -> intel_enable_transcoder -> plane update).
        for reg in [d::DDI_BUF_CTL_B, d::DDI_BUF_CTL_A] {
            let value = self.saved(reg)?;
            masked_write(self.device, reg, value)?;
            if value & ENABLE != 0 {
                wait(self.device, timer, reg, 1 << 7, 0)?;
            }
        }
        masked_write(self.device, p::PIPECONF_A, self.saved(p::PIPECONF_A)?)?;
        wait(self.device, timer, p::PIPECONF_A, RUNNING, RUNNING)?;
        masked_write(self.device, p::PLANE_CTL_A, self.saved(p::PLANE_CTL_A)?)?;
        masked_write(self.device, p::PLANE_SURF_A, self.saved(p::PLANE_SURF_A)?)?;
        // Withdraw only driver-owned requests/policies, never BIOS/debug/KVMR.
        // DC policy last, after all requested wells and DBUF are reinstated.
        for &reg in journal.touched.iter().rev() {
            if policy(reg).0 == Class::Power && reg != regs::DC_STATE_EN {
                masked_write(self.device, reg, self.saved(reg)?)?;
            }
        }
        if journal.touched.contains(&regs::DC_STATE_EN) {
            masked_write(
                self.device,
                regs::DC_STATE_EN,
                self.saved(regs::DC_STATE_EN)?,
            )?;
        }
        for &reg in &journal.touched {
            let (class, mask) = policy(reg);
            if class != Class::Transient
                && read(self.device, reg)? & mask != self.saved(reg)? & mask
            {
                return Err(format!("{} final restore mismatch", reg.name()));
            }
        }
        // Verify the original clock image, whether or not a source-ordered
        // runtime change had to be reversed above.
        for reg in [regs::CDCLK_CTL, regs::CDCLK_PLL_ENABLE] {
            let mask = if reg == regs::CDCLK_PLL_ENABLE {
                !LOCK
            } else {
                u32::MAX
            };
            if read(self.device, reg)? & mask != self.saved(reg)? & mask {
                return Err(String::from("firmware CDCLK image changed"));
            }
        }
        gtt.verify_checkpoint(image).map_err(|e| e.describe())?;
        self.firmware.verify(self.device, timer)
    }
}
impl<R: Registers> Registers for Transaction<'_, R> {
    fn read(&self, reg: Register) -> Option<u32> {
        snapshot_read32(self.device, reg)
    }
    fn read64(&self, reg: Register) -> Option<u64> {
        self.device.read64(reg)
    }
    fn write(&self, reg: Register, value: u32) -> bool {
        let mut journal = self.journal.lock();
        let other_pll = if self.pll_id == 0 {
            [pll::DPLL1_ENABLE, pll::DPLL1_CFGCR0, pll::DPLL1_CFGCR1]
        } else {
            [pll::DPLL0_ENABLE, pll::DPLL0_CFGCR0, pll::DPLL0_CFGCR1]
        };
        let foreign_phy = is_phy(reg)
            && reg
                .name()
                .ends_with(if self.ddi == Ddi::A { "(B)" } else { "(A)" });
        let foreign_pipe = (0x71000..0x74000).contains(&reg.offset())
            || (0x61000..0x64000).contains(&reg.offset());
        if journal.failed
            || policy(reg).0 == Class::Forbidden
            || self.saved(reg).is_err()
            || other_pll.contains(&reg)
            || foreign_phy
            || foreign_pipe
        {
            journal.failed = true;
            return false;
        }
        if !journal.touched.contains(&reg) {
            if let Some(index) = DW5_GROUPS.iter().position(|group| *group == reg) {
                for lane in LANE_DW5[index] {
                    if self.saved(lane).is_err() {
                        journal.failed = true;
                        return false;
                    }
                    if !journal.touched.contains(&lane) {
                        journal.touched.push(lane);
                    }
                }
            }
            journal.touched.push(reg);
        }
        journal.attempts += 1;
        if journal.fail_at == Some(journal.attempts) {
            journal.failed = true;
            return false;
        }
        let result = if policy(reg).0 == Class::Transient {
            self.device.write(reg, value)
        } else {
            masked_write(self.device, reg, value).is_ok()
        };
        if !result {
            journal.failed = true;
        }
        result
    }
}
