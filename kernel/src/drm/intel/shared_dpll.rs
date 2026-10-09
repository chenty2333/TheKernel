// SPDX-License-Identifier: MIT
//! Kernel-side adapter for the translated Gen12/13 shared-DPLL manager.
//!
//! This module intentionally admits only the modeled Alder Lake-N display-13
//! family.  It exposes the translated reservation/readout and generic
//! `intel_dpll_enable()` / `intel_dpll_disable()` lifecycle behind the kernel's
//! typed MMIO, DKL selector lock, and refcounted display power state.  The
//! caller remains responsible for the larger atomic commit and its complete
//! modeset rollback image.

use intel_display::{
    dmc::DmcPlatform,
    intel_dpll_mgr_full as dpll,
    power_map::{
        self, DomainList, PowerDomain as KernelPowerDomain, PowerWellInstance, WellControl, WellOps,
    },
};
use spin::{Mutex, MutexGuard};

use crate::drm::intel::{
    gmbus::PollTimer,
    id::{self, DisplayDevice, DisplayStepping, Generation},
    power::PowerState,
    regs::{self, Meaning, Register, Registers},
};

const ADLN_DEVICE_ID_FIRST: u16 = 0x46d0;
const ADLN_DEVICE_ID_LAST: u16 = 0x46d4;
const MAX_WAIT_POLLS_PER_MS: u32 = 10_000;
const POWER_REF_SLOTS: usize = 16;

/// Serializes manager/atomic state until the DRM layer has a native connection
/// mutex.  DKL selector traffic still uses the existing shared HIP lock below.
static DPLL_CONNECTION_LOCK: Mutex<()> = Mutex::new(());
static DPLL_HW_LOCK: Mutex<()> = Mutex::new(());

// These two source-confirmed controls are not yet in regs/dpll.rs.  Keep the
// narrow declarations local rather than manufacturing a wider register map:
// i915's PORTTC1/2_PLL_ENABLE are 0x46038/0x46040; TC3/4 are deliberately not
// admitted by this N305 adapter.
const PORTTC1_PLL_ENABLE: Register =
    Register::read_write("PORTTC1_PLL_ENABLE", 0x4_6038, Meaning::BringUp, None);
const PORTTC2_PLL_ENABLE: Register =
    Register::read_write("PORTTC2_PLL_ENABLE", 0x4_6040, Meaning::BringUp, None);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DpllFailure {
    UnsupportedIdentity,
    UnknownStepping,
    Uninitialized,
    AlreadyInitialized,
    Quarantined,
    UnsupportedRegister,
    RegisterUnavailable,
    DklAccessUnavailable,
    DklPortNotPowered,
    PowerDomain,
    PowerRefLedgerFull,
    InvalidPowerRef,
    WaitTimeout,
    UnexpectedState,
    ManagerRejected(i32),
    InvalidCrtc,
    RollbackNotProven,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AdlNIdentity {
    device_id: u16,
    revision: u8,
    stepping: DisplayStepping,
}

impl AdlNIdentity {
    /// Build the token only from a PCI identity in the kernel's ADL-N device
    /// table and a revision the table maps exactly.  Unknown/future revisions
    /// are rejected before any DPLL state or MMIO is touched.
    pub(crate) fn verify(
        vendor_id: u16,
        device_id: u16,
        revision: u8,
    ) -> Result<Self, DpllFailure> {
        if vendor_id != 0x8086 || !(ADLN_DEVICE_ID_FIRST..=ADLN_DEVICE_ID_LAST).contains(&device_id)
        {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        let Some(device) = id::DEVICES
            .iter()
            .find(|device| device.device_id == device_id)
        else {
            return Err(DpllFailure::UnsupportedIdentity);
        };
        if !is_adln_device(device) {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        let stepping = device.stepping(revision);
        if !stepping.is_known() {
            return Err(DpllFailure::UnknownStepping);
        }
        Ok(Self {
            device_id,
            revision,
            stepping,
        })
    }
}

fn is_adln_device(device: &DisplayDevice) -> bool {
    device.generation == Generation::Gen12
        && device.display_version == Some(13)
        && device.name == "Alder Lake-N integrated graphics"
        && (ADLN_DEVICE_ID_FIRST..=ADLN_DEVICE_ID_LAST).contains(&device.device_id)
}

#[derive(Clone, Copy, Debug)]
struct PowerRef {
    cookie: u64,
    domain: dpll::PowerDomain,
}

/// Backend boundary for the translated manager's wakeref hooks. The pin
/// implementation is read-only and owns no power-well counters; the existing
/// PowerState implementation remains available for callers that own those
/// domains through the kernel's refcounted power manager.
pub(crate) trait DpllPowerAccess<R: Registers> {
    fn reference_clock_khz(&self, registers: &R) -> Result<u32, DpllFailure>;
    fn begin_access(&mut self, registers: &R, hold_display_core: bool)
    -> Result<bool, DpllFailure>;
    fn end_access(&mut self, registers: &R, core_cookie: bool) -> Result<(), DpllFailure>;
    fn power_get(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure>;
    fn power_get_if_enabled(
        &mut self,
        registers: &R,
        domain: dpll::PowerDomain,
    ) -> Result<bool, DpllFailure>;
    fn power_put(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure>;
    fn tc_port_held(&self, registers: &R, port: dpll::TcPort) -> bool;
    fn verify_access(&self, registers: &R) -> Result<(), DpllFailure>;
}

impl<R: Registers> DpllPowerAccess<R> for PowerState {
    fn reference_clock_khz(&self, _registers: &R) -> Result<u32, DpllFailure> {
        let observation = self.cdclk.observed;
        if !observation.reference_recognised {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        let reference_khz = observation.reference.khz();
        if matches!(reference_khz, 19_200 | 24_000 | 38_400) {
            Ok(reference_khz)
        } else {
            Err(DpllFailure::UnsupportedIdentity)
        }
    }

    fn begin_access(
        &mut self,
        registers: &R,
        hold_display_core: bool,
    ) -> Result<bool, DpllFailure> {
        if hold_display_core {
            self.get_domain(registers, KernelPowerDomain::DisplayCore)
                .map_err(|_| DpllFailure::PowerDomain)?;
        }
        Ok(hold_display_core)
    }

    fn end_access(&mut self, registers: &R, core_cookie: bool) -> Result<(), DpllFailure> {
        if core_cookie {
            self.put_domain(registers, KernelPowerDomain::DisplayCore)
                .map_err(|_| DpllFailure::PowerDomain)?;
        }
        Ok(())
    }

    fn power_get(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure> {
        self.get_domain(registers, kernel_power_domain(domain)?)
            .map_err(|_| DpllFailure::PowerDomain)
    }

    fn power_get_if_enabled(
        &mut self,
        registers: &R,
        domain: dpll::PowerDomain,
    ) -> Result<bool, DpllFailure> {
        self.get_domain_if_enabled(registers, kernel_power_domain(domain)?, true)
            .map_err(|_| DpllFailure::PowerDomain)
    }

    fn power_put(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure> {
        self.put_domain(registers, kernel_power_domain(domain)?)
            .map_err(|_| DpllFailure::PowerDomain)
    }

    fn tc_port_held(&self, _registers: &R, port: dpll::TcPort) -> bool {
        tc_domains_held_in_power_state(self, port)
    }

    fn verify_access(&self, _registers: &R) -> Result<(), DpllFailure> {
        Ok(())
    }
}

fn kernel_power_domain(domain: dpll::PowerDomain) -> Result<KernelPowerDomain, DpllFailure> {
    match domain {
        dpll::PowerDomain::DisplayCore => Ok(KernelPowerDomain::DisplayCore),
        dpll::PowerDomain::DcOff => Ok(KernelPowerDomain::DcOff),
    }
}

/// Read-only DPLL power/clock view backed by the already-held fastboot pin.
/// Construct one for each operation from the Native-owned `PowerPin`; it does
/// not outlive or replace that pin and never requests/clears a well.
pub(crate) struct PinnedDpllPower<'a> {
    pin: &'a crate::drm::intel::fastboot::PowerPin,
    identity: AdlNIdentity,
    reference_khz: u32,
}

impl<'a> PinnedDpllPower<'a> {
    pub(crate) fn new(
        pin: &'a crate::drm::intel::fastboot::PowerPin,
        identity: AdlNIdentity,
        reference_khz: u32,
    ) -> Result<Self, DpllFailure> {
        if identity.stepping != DisplayStepping::D0 {
            return Err(DpllFailure::UnknownStepping);
        }
        if !matches!(reference_khz, 19_200 | 24_000 | 38_400) {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        verify_adln_pin_map(manager_tc_port(pin.port())?)?;
        Ok(Self {
            pin,
            identity,
            reference_khz,
        })
    }

    fn verify<R: Registers>(&self, registers: &R) -> Result<(), DpllFailure> {
        if self.identity.stepping != DisplayStepping::D0 {
            return Err(DpllFailure::UnknownStepping);
        }
        verify_adln_pin_map(manager_tc_port(self.pin.port())?)?;
        self.pin
            .verify_dpll_context(registers, self.pin.port(), self.reference_khz)
            .map_err(|_| DpllFailure::DklAccessUnavailable)
    }
}

impl<R: Registers> DpllPowerAccess<R> for PinnedDpllPower<'_> {
    fn reference_clock_khz(&self, registers: &R) -> Result<u32, DpllFailure> {
        self.verify(registers)?;
        Ok(self.reference_khz)
    }

    fn begin_access(
        &mut self,
        registers: &R,
        _hold_display_core: bool,
    ) -> Result<bool, DpllFailure> {
        self.verify(registers)?;
        Ok(false) // PowerPin owns the physical wells for the whole KMS lifetime.
    }

    fn end_access(&mut self, registers: &R, core_cookie: bool) -> Result<(), DpllFailure> {
        if core_cookie {
            return Err(DpllFailure::InvalidPowerRef);
        }
        self.verify(registers)
    }

    fn power_get(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure> {
        if domain != dpll::PowerDomain::DisplayCore {
            return Err(DpllFailure::PowerDomain);
        }
        self.verify(registers)
    }

    fn power_get_if_enabled(
        &mut self,
        registers: &R,
        domain: dpll::PowerDomain,
    ) -> Result<bool, DpllFailure> {
        if domain != dpll::PowerDomain::DisplayCore {
            return Err(DpllFailure::PowerDomain);
        }
        self.verify(registers)?;
        Ok(true)
    }

    fn power_put(&mut self, registers: &R, domain: dpll::PowerDomain) -> Result<(), DpllFailure> {
        if domain != dpll::PowerDomain::DisplayCore {
            return Err(DpllFailure::PowerDomain);
        }
        self.verify(registers)
    }

    fn tc_port_held(&self, registers: &R, port: dpll::TcPort) -> bool {
        manager_tc_port(self.pin.port()) == Ok(port) && self.verify(registers).is_ok()
    }

    fn verify_access(&self, registers: &R) -> Result<(), DpllFailure> {
        self.verify(registers)
    }
}

fn instance_has_domain(instance: PowerWellInstance, domain: KernelPowerDomain) -> bool {
    match instance.domains {
        DomainList::All => true,
        DomainList::None => false,
        DomainList::Set(domains) => domains.contains(&domain),
    }
}

fn manager_tc_port(port: intel_display::dkl_phy::TcPort) -> Result<dpll::TcPort, DpllFailure> {
    match port {
        intel_display::dkl_phy::TcPort::Tc1 => Ok(dpll::TcPort::Tc1),
        intel_display::dkl_phy::TcPort::Tc2 => Ok(dpll::TcPort::Tc2),
        _ => Err(DpllFailure::UnsupportedIdentity),
    }
}

/// Compare the registers independently captured by the fastboot firmware
/// readout with the common DKL fields returned by the translated shared-DPLL
/// dispatcher. Preserve source-manager-only fields/reserved bits from the
/// dispatched image, while comparing the documented DKL register masks.
pub(crate) fn dkl_state_matches_source_readout(
    firmware: &intel_display::dpll_mgr::DklPllState,
    dispatched: &dpll::IntelDpllHwState,
    afc_startup_override: bool,
) -> bool {
    let mut expected = *dispatched;
    let actual = dispatched.icl;
    let expected_icl = &mut expected.icl;
    let div0_mask = 0x1f_ffff | if afc_startup_override { 7 << 25 } else { 0 };
    let div1_mask = (31 << 16) | 0xff;
    let ssc_mask = (7 << 29) | (0xff << 16) | (7 << 11) | (1 << 9);
    let bias_mask = ((1 << 30) | (0x3f_ffff << 8)) & actual.mg_pll_bias_mask;
    let tdc_mask = 0xffff & actual.mg_pll_tdc_coldst_bias_mask;

    expected_icl.mg_refclkin_ctl = firmware.refclkin_ctl;
    expected_icl.mg_clktop2_coreclkctl1 = firmware.coreclkctl1;
    expected_icl.mg_clktop2_hsclkctl = firmware.hsclkctl;
    expected_icl.mg_pll_div0 = (actual.mg_pll_div0 & !div0_mask) | (firmware.div0 & div0_mask);
    expected_icl.mg_pll_div1 = (actual.mg_pll_div1 & !div1_mask) | (firmware.div1 & div1_mask);
    expected_icl.mg_pll_ssc = (actual.mg_pll_ssc & !ssc_mask) | (firmware.ssc & ssc_mask);
    expected_icl.mg_pll_bias = (actual.mg_pll_bias & !bias_mask) | (firmware.bias & bias_mask);
    expected_icl.mg_pll_tdc_coldst_bias =
        (actual.mg_pll_tdc_coldst_bias & !tdc_mask) | (firmware.tdc_coldst_bias & tdc_mask);
    dpll::icl_compare_hw_state(&expected, dispatched)
}

fn verify_adln_pin_map(port: dpll::TcPort) -> Result<(), DpllFailure> {
    let (lane, io, aux, ddi_control, aux_control) = match port {
        dpll::TcPort::Tc1 => (
            KernelPowerDomain::PortDdiLanesTc1,
            KernelPowerDomain::PortDdiIoTc1,
            KernelPowerDomain::AuxUsbc1,
            WellControl::TglDdiTc1,
            WellControl::TglAuxTc1,
        ),
        dpll::TcPort::Tc2 => (
            KernelPowerDomain::PortDdiLanesTc2,
            KernelPowerDomain::PortDdiIoTc2,
            KernelPowerDomain::AuxUsbc2,
            WellControl::TglDdiTc2,
            WellControl::TglAuxTc2,
        ),
        _ => return Err(DpllFailure::UnsupportedIdentity),
    };
    let map = power_map::power_wells(DmcPlatform::AlderLakeN);
    let mut pw2 = 0;
    let mut dc_off = 0;
    let mut ddi = 0;
    let mut aux_pw2 = 0;
    let mut aux_dc_off = 0;
    let mut aux_controller = 0;
    let mut pw1 = 0;
    let mut pwa = 0;
    for group in map {
        for instance in group.instances.iter().copied() {
            if instance.always_on {
                continue;
            }
            if instance_has_domain(instance, lane) {
                match (group.ops, instance.control) {
                    (WellOps::Hsw, Some(WellControl::IclPw2)) => pw2 += 1,
                    (WellOps::DcOff, None) => dc_off += 1,
                    _ => return Err(DpllFailure::UnsupportedRegister),
                }
            }
            if instance_has_domain(instance, io) {
                if group.ops == WellOps::Ddi && instance.control == Some(ddi_control) {
                    ddi += 1;
                } else {
                    return Err(DpllFailure::UnsupportedRegister);
                }
            }
            if instance_has_domain(instance, aux) {
                match (group.ops, instance.control) {
                    (WellOps::Hsw, Some(WellControl::IclPw2)) => aux_pw2 += 1,
                    (WellOps::DcOff, None) => aux_dc_off += 1,
                    (WellOps::Aux, Some(control)) if control == aux_control => aux_controller += 1,
                    _ => return Err(DpllFailure::UnsupportedRegister),
                }
            }
            if instance.control == Some(WellControl::IclPw1) {
                pw1 += 1;
            }
            if instance.control == Some(WellControl::XelpdPwA) {
                pwa += 1;
            }
        }
    }
    if [
        pw2,
        dc_off,
        ddi,
        aux_pw2,
        aux_dc_off,
        aux_controller,
        pw1,
        pwa,
    ] == [1, 1, 1, 1, 1, 1, 1, 1]
    {
        Ok(())
    } else {
        Err(DpllFailure::UnsupportedRegister)
    }
}

/// Persisted shared-DPLL state and power-reference ledger for one verified
/// ADL-N display.  Keep this object with the DRM device, not on the stack:
/// `intel_dpll_enable()`'s active masks and the CRTC reservation state survive
/// across atomic transactions.
pub(crate) struct SharedDpllState {
    identity: AdlNIdentity,
    display: dpll::IntelDpllDisplay,
    power_refs: [Option<PowerRef>; POWER_REF_SLOTS],
    next_cookie: u64,
    initialized: bool,
    readout_done: bool,
    quarantined: bool,
    last_debug_count: u32,
    last_warning_count: u32,
    last_failure: Option<DpllFailure>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SharedDpllReport {
    pub(crate) device_id: u16,
    pub(crate) revision: u8,
    pub(crate) stepping: DisplayStepping,
    pub(crate) num_dpll: usize,
    pub(crate) readout_done: bool,
    pub(crate) quarantined: bool,
    pub(crate) outstanding_power_refs: usize,
    pub(crate) display_core_refs: u32,
    pub(crate) dc_off_refs: u32,
    pub(crate) debug_events: u32,
    pub(crate) warnings: u32,
    pub(crate) last_failure: Option<DpllFailure>,
    pub(crate) dpll_masks: [u8; dpll::MAX_DPLLS],
    pub(crate) dpll_on: [bool; dpll::MAX_DPLLS],
    pub(crate) dpll_reserved_pipes: [u8; dpll::MAX_DPLLS],
}

/// Software manager checkpoint paired with a caller's verified hardware
/// checkpoint. This restores manager masks only after the outer modeset layer
/// has proved its MMIO before-image rollback; it is not a substitute for it.
pub(crate) struct SharedDpllUndo {
    display: dpll::IntelDpllDisplay,
}

impl SharedDpllState {
    pub(crate) fn new(identity: AdlNIdentity, display_id: usize) -> Self {
        let mut display = dpll::IntelDpllDisplay::default();
        display.display_id = display_id;
        display.display_ver = 13;
        display.platform.alderlake_p = true;
        Self {
            identity,
            display,
            power_refs: [None; POWER_REF_SLOTS],
            next_cookie: 1,
            initialized: false,
            readout_done: false,
            quarantined: false,
            last_debug_count: 0,
            last_warning_count: 0,
            last_failure: None,
        }
    }

    pub(crate) fn report(&self) -> SharedDpllReport {
        let mut dpll_masks = [0; dpll::MAX_DPLLS];
        let mut dpll_on = [false; dpll::MAX_DPLLS];
        let mut dpll_reserved_pipes = [0; dpll::MAX_DPLLS];
        for (index, pll) in self
            .display
            .dplls
            .iter()
            .take(self.display.num_dpll)
            .enumerate()
        {
            dpll_masks[index] = pll.active_mask;
            dpll_on[index] = pll.on;
            dpll_reserved_pipes[index] = pll.state.pipe_mask;
        }
        let refs_for = |domain| {
            self.power_refs
                .iter()
                .flatten()
                .filter(|reference| reference.domain == domain)
                .count() as u32
        };
        SharedDpllReport {
            device_id: self.identity.device_id,
            revision: self.identity.revision,
            stepping: self.identity.stepping,
            num_dpll: self.display.num_dpll,
            readout_done: self.readout_done,
            quarantined: self.quarantined,
            outstanding_power_refs: self.power_refs.iter().flatten().count(),
            display_core_refs: refs_for(dpll::PowerDomain::DisplayCore),
            dc_off_refs: refs_for(dpll::PowerDomain::DcOff),
            debug_events: self.last_debug_count,
            warnings: self.last_warning_count,
            last_failure: self.last_failure,
            dpll_masks,
            dpll_on,
            dpll_reserved_pipes,
        }
    }

    fn ensure_usable(&self) -> Result<(), DpllFailure> {
        if !self.initialized {
            return Err(DpllFailure::Uninitialized);
        }
        if self.quarantined {
            return Err(DpllFailure::Quarantined);
        }
        Ok(())
    }

    fn refresh_reference_clock<R: Registers, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        power: &P,
    ) -> Result<(), DpllFailure> {
        let reference_khz = power.reference_clock_khz(registers)?;
        self.display.cdclk_ref = reference_khz;
        dpll::intel_dpll_update_ref_clks(&mut self.display);
        Ok(())
    }

    /// Initialize the translated ADLP manager.  N305 uses the source-verified
    /// combo PLLs, TBT PLL, and DKL TC1/TC2 PLLs; manager entries for TC3/TC4
    /// are excluded because their N305 enable-register mapping is not proven.
    pub(crate) fn init<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
    ) -> Result<(), DpllFailure> {
        if self.initialized {
            return Err(DpllFailure::AlreadyInitialized);
        }
        self.refresh_reference_clock(registers, power)?;
        let result = self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            dpll::intel_dpll_init(hooks, display);
            // ADLP's table order is DPLL0, DPLL1, TBT, TC1, TC2, TC3, TC4.
            if display.mgr.is_none() || display.num_dpll < 5 {
                hooks.fail(DpllFailure::UnsupportedIdentity);
                return;
            }
            display.num_dpll = 5;
        });
        result?;
        self.initialized = true;
        Ok(())
    }

    /// Restrict the translated ADLP manager to the Type-C PLLs whose AUX/PHY
    /// access can be proven by this single-port N305 lease. DPLL0/1 and TBT
    /// stay in source order; TC2 is moved into the final active slot so the
    /// unpowered sibling TC PLL is not read or considered by allocator scans.
    pub(crate) fn limit_to_tc_port(
        &mut self,
        port: intel_display::dkl_phy::TcPort,
    ) -> Result<usize, DpllFailure> {
        self.ensure_usable()?;
        if self.readout_done || self.display.num_dpll != 5 {
            return Err(DpllFailure::AlreadyInitialized);
        }
        match port {
            intel_display::dkl_phy::TcPort::Tc1 => {}
            intel_display::dkl_phy::TcPort::Tc2 => {
                self.display.dplls.swap(3, 4);
                self.display.dplls[3].index = 3;
                self.display.dplls[4].index = 4;
            }
            _ => return Err(DpllFailure::UnsupportedIdentity),
        }
        self.display.num_dpll = 4;
        Ok(3)
    }

    /// Read hardware state once from caller-proved CRTC readouts.  The source
    /// manager dispatches DKL TC1/TC2 through `intel_dpll_get_hw_state()` to
    /// its internal `dkl_pll_get_hw_state()` implementation.
    pub(crate) fn readout<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        crtcs: [dpll::CrtcState; dpll::MAX_PIPES],
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        if self.readout_done {
            return Err(DpllFailure::AlreadyInitialized);
        }
        self.refresh_reference_clock(registers, power)?;
        for crtc in crtcs.iter().filter(|crtc| crtc.hw_active) {
            self.check_crtc_pll_power_domains(registers, power, crtc)?;
        }
        let result = self.with_hooks(registers, timer, power, false, true, |hooks, display| {
            display.crtc_states = crtcs;
            dpll::intel_dpll_readout_hw_state(hooks, display);
        });
        result?;
        self.readout_done = true;
        Ok(())
    }

    /// Query one manager-owned PLL through the public generic hardware-state
    /// dispatcher.  On ADL-N this reaches the internal DKL readout for TC1/2;
    /// callers do not need a parallel hand-written DKL read path.
    pub(crate) fn get_hw_state<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        pll_index: usize,
    ) -> Result<(bool, dpll::IntelDpllHwState), DpllFailure> {
        self.ensure_usable()?;
        if pll_index >= self.display.num_dpll {
            return Err(DpllFailure::InvalidCrtc);
        }
        self.refresh_reference_clock(registers, power)?;
        self.with_hooks(registers, timer, power, false, true, |hooks, display| {
            let mut state = dpll::IntelDpllHwState::default();
            let on = dpll::intel_dpll_get_hw_state(hooks, display, pll_index, &mut state);
            (on, state)
        })
    }

    /// Source-order sanitize after the readout has established every active
    /// CRTC reference.  Any hook warning that signals unexpected state becomes
    /// a hard error and quarantines this manager rather than disabling a PLL
    /// whose owner/reference state is uncertain.
    pub(crate) fn sanitize<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        if !self.readout_done {
            return Err(DpllFailure::Uninitialized);
        }
        self.refresh_reference_clock(registers, power)?;
        self.with_hooks(registers, timer, power, true, true, |hooks, display| {
            dpll::intel_dpll_sanitize_state(hooks, display);
        })
    }

    pub(crate) fn compute<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        atomic: &mut dpll::IntelAtomicState,
        crtc: &dpll::IntelCrtc,
        encoder: &dpll::IntelEncoder,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(registers, power)?;
        self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            let error = dpll::intel_dpll_compute(hooks, display, atomic, crtc, encoder);
            if error != 0 {
                hooks.fail(DpllFailure::ManagerRejected(error));
            }
        })
    }

    pub(crate) fn reserve<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        atomic: &mut dpll::IntelAtomicState,
        crtc: &dpll::IntelCrtc,
        encoder: &dpll::IntelEncoder,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(registers, power)?;
        self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            let error = dpll::intel_dpll_reserve(hooks, display, atomic, crtc, encoder);
            if error != 0 {
                hooks.fail(DpllFailure::ManagerRejected(error));
            }
        })
    }

    pub(crate) fn release<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        atomic: &mut dpll::IntelAtomicState,
        crtc: &dpll::IntelCrtc,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            dpll::intel_dpll_release(hooks, display, atomic, crtc);
        })
    }

    /// Swap a checked atomic DPLL state into the manager before hardware enable
    /// callbacks (the generic source enable path validates its reservation
    /// against this manager state). Keep the returned token until the enclosing
    /// hardware commit is irrevocable or its checkpoint has restored the old
    /// state.
    pub(crate) fn swap_atomic(
        &mut self,
        atomic: &mut dpll::IntelAtomicState,
    ) -> Result<SharedDpllUndo, DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        let _connection = DPLL_CONNECTION_LOCK.lock();
        if self.power_refs.iter().any(Option::is_some) {
            return Err(DpllFailure::InvalidPowerRef);
        }
        let undo = SharedDpllUndo {
            display: self.display.clone(),
        };
        dpll::intel_dpll_swap_state(&mut self.display, atomic);
        Ok(undo)
    }

    /// Restore manager state only after the caller's modeset checkpoint has
    /// fully restored and verified the prior hardware image. A leaked logical
    /// wakeref cookie keeps this manager quarantined.
    pub(crate) fn restore_after_verified_hardware_rollback(
        &mut self,
        undo: SharedDpllUndo,
    ) -> Result<(), DpllFailure> {
        let _connection = DPLL_CONNECTION_LOCK.lock();
        if self.power_refs.iter().any(Option::is_some) {
            self.last_failure = Some(DpllFailure::InvalidPowerRef);
            self.quarantined = true;
            return Err(DpllFailure::InvalidPowerRef);
        }
        self.display = undo.display;
        self.quarantined = false;
        self.last_failure = None;
        Ok(())
    }

    /// Enable the reserved PLL selected by `intel_dpll_reserve()`.  This is
    /// the generic path used for DKL TC1/TC2; no direct `dkl_pll_on()` call is
    /// needed.  On post-write error state is quarantined.  Hardware rollback
    /// remains the caller's outer modeset checkpoint responsibility.
    pub(crate) fn enable<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        crtc_state: &dpll::CrtcState,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(registers, power)?;
        self.check_crtc_pll_power_domains(registers, power, crtc_state)?;
        self.with_hooks(registers, timer, power, true, true, |hooks, display| {
            dpll::intel_dpll_enable(hooks, display, crtc_state);
            if let Some(index) = crtc_state.intel_dpll {
                if display
                    .dplls
                    .get(index)
                    .is_none_or(|pll| !pll.on || pll.active_mask & crtc_state.joined_pipe_mask == 0)
                {
                    hooks.fail(DpllFailure::UnexpectedState);
                }
            }
        })
    }

    /// Disable the active shared PLL using the generic translated manager.
    /// A timeout/uncertain refcount quarantines the state for the enclosing
    /// modeset rollback; this method never reports a guessed successful unwind.
    pub(crate) fn disable<R: Registers, T: PollTimer, P: DpllPowerAccess<R>>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        crtc_state: &dpll::CrtcState,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(registers, power)?;
        self.check_crtc_pll_power_domains(registers, power, crtc_state)?;
        self.with_hooks(registers, timer, power, true, true, |hooks, display| {
            dpll::intel_dpll_disable(hooks, display, crtc_state);
            if let Some(index) = crtc_state.intel_dpll {
                if let Some(pll) = display.dplls.get(index) {
                    if pll.active_mask == 0 && pll.on {
                        hooks.fail(DpllFailure::UnexpectedState);
                    }
                }
            }
        })
    }

    fn check_crtc_pll_power_domains<R: Registers, P: DpllPowerAccess<R>>(
        &self,
        registers: &R,
        power: &P,
        crtc: &dpll::CrtcState,
    ) -> Result<(), DpllFailure> {
        if crtc.pipe as usize >= dpll::MAX_PIPES || crtc.joined_pipe_mask == 0 {
            return Err(DpllFailure::InvalidCrtc);
        }
        let Some(index) = crtc.intel_dpll else {
            return Err(DpllFailure::InvalidCrtc);
        };
        let Some(pll) = self.display.dplls.get(index) else {
            return Err(DpllFailure::InvalidCrtc);
        };
        let Some(info) = pll.info else {
            return Err(DpllFailure::InvalidCrtc);
        };
        match (crtc.port, info.funcs, info.id.0) {
            (dpll::Port::A, dpll::DpllFunction::Combo, 0)
            | (dpll::Port::B, dpll::DpllFunction::Combo, 1) => Ok(()),
            (dpll::Port::C | dpll::Port::Tc(dpll::TcPort::Tc1), dpll::DpllFunction::Dkl, 3) => {
                if power.tc_port_held(registers, dpll::TcPort::Tc1) {
                    Ok(())
                } else {
                    Err(DpllFailure::DklPortNotPowered)
                }
            }
            (dpll::Port::D | dpll::Port::Tc(dpll::TcPort::Tc2), dpll::DpllFunction::Dkl, 4) => {
                if power.tc_port_held(registers, dpll::TcPort::Tc2) {
                    Ok(())
                } else {
                    Err(DpllFailure::DklPortNotPowered)
                }
            }
            // TC legacy active CRTCs use the DKL PLL, not the reserved TBT
            // alternate PLL; the generic CRTC lifecycle owns the active DPLL.
            _ => Err(DpllFailure::InvalidCrtc),
        }
    }

    fn ensure_readout(&self) -> Result<(), DpllFailure> {
        if self.readout_done {
            Ok(())
        } else {
            Err(DpllFailure::Uninitialized)
        }
    }

    fn with_hooks<R, T, P, O>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut P,
        hold_display_core: bool,
        quarantine_on_fault: bool,
        operation: impl FnOnce(&mut KernelDpllHooks<'_, R, T, P>, &mut dpll::IntelDpllDisplay) -> O,
    ) -> Result<O, DpllFailure>
    where
        R: Registers,
        T: PollTimer,
        P: DpllPowerAccess<R>,
    {
        if self.quarantined {
            return Err(DpllFailure::Quarantined);
        }
        let _connection = DPLL_CONNECTION_LOCK.lock();
        let core_cookie = power.begin_access(registers, hold_display_core)?;
        let mut hooks = KernelDpllHooks {
            registers,
            timer,
            power,
            power_refs: &mut self.power_refs,
            next_cookie: &mut self.next_cookie,
            fault: None,
            debug_count: 0,
            warning_count: 0,
            writes: 0,
            connection_locked: true,
            dpll_lock_slot: None,
        };
        let output = operation(&mut hooks, &mut self.display);
        let fault = hooks.fault;
        self.last_debug_count = self.last_debug_count.saturating_add(hooks.debug_count);
        self.last_warning_count = self.last_warning_count.saturating_add(hooks.warning_count);
        if fault.is_some() {
            self.last_failure = fault;
        }
        let writes = hooks.writes;
        drop(hooks);

        let release_error = power.end_access(registers, core_cookie).err();
        if fault.is_some() || release_error.is_some() {
            if quarantine_on_fault || writes != 0 || release_error.is_some() {
                self.quarantined = true;
            }
            if release_error.is_some() {
                self.last_failure = Some(DpllFailure::PowerDomain);
            }
            let failure = if writes != 0 {
                // The source translation returns void for hardware steps and
                // supplies no inverse sequence. Do not call a partial write a
                // rollback; the enclosing modeset checkpoint must restore it.
                DpllFailure::RollbackNotProven
            } else if release_error.is_some() {
                DpllFailure::PowerDomain
            } else {
                fault.unwrap_or(DpllFailure::UnexpectedState)
            };
            return Err(failure);
        }
        self.last_failure = None;
        Ok(output)
    }
}

struct KernelDpllHooks<'a, R, T, P> {
    registers: &'a R,
    timer: &'a T,
    power: &'a mut P,
    power_refs: &'a mut [Option<PowerRef>; POWER_REF_SLOTS],
    next_cookie: &'a mut u64,
    fault: Option<DpllFailure>,
    debug_count: u32,
    warning_count: u32,
    writes: u32,
    connection_locked: bool,
    dpll_lock_slot: Option<MutexGuard<'static, ()>>,
}

impl<R: Registers, T: PollTimer, P: DpllPowerAccess<R>> KernelDpllHooks<'_, R, T, P> {
    fn fail(&mut self, failure: DpllFailure) {
        if self.fault.is_none() {
            self.fault = Some(failure);
        }
    }

    fn register(register: dpll::DpllRegister, write: bool) -> Option<Register> {
        use dpll::{CfgRegisterFamily as Family, DpllId, DpllRegister as Dr, MgRegister, TcPort};
        let mapped = match register {
            Dr::ComboEnable(dpll::ComboEnableFamily::Icl, DpllId(0)) => {
                Some(regs::dpll::DPLL0_ENABLE)
            }
            Dr::ComboEnable(dpll::ComboEnableFamily::Icl, DpllId(1)) => {
                Some(regs::dpll::DPLL1_ENABLE)
            }
            Dr::TbtEnable => Some(regs::dpll::TBT_PLL_ENABLE),
            Dr::AdlpTcEnable(TcPort::Tc1) => Some(PORTTC1_PLL_ENABLE),
            Dr::AdlpTcEnable(TcPort::Tc2) => Some(PORTTC2_PLL_ENABLE),
            Dr::CfgCr0(Family::TigerLake, DpllId(0)) => Some(regs::dpll::DPLL0_CFGCR0),
            Dr::CfgCr1(Family::TigerLake, DpllId(0)) => Some(regs::dpll::DPLL0_CFGCR1),
            Dr::CfgCr0(Family::TigerLake, DpllId(1)) => Some(regs::dpll::DPLL1_CFGCR0),
            Dr::CfgCr1(Family::TigerLake, DpllId(1)) => Some(regs::dpll::DPLL1_CFGCR1),
            Dr::CfgCr0(Family::TigerLake, DpllId(2)) => Some(regs::dpll::TBT_PLL_CFGCR0),
            Dr::CfgCr1(Family::TigerLake, DpllId(2)) => Some(regs::dpll::TBT_PLL_CFGCR1),
            // ADL-N's exact manager path does not use MG direct registers,
            // DG1 enable family, TC3/4 enables, or AFC override DIV0.
            Dr::MgEnable(_)
            | Dr::ComboEnable(..)
            | Dr::AdlpTcEnable(_)
            | Dr::CfgCr0(..)
            | Dr::CfgCr1(..)
            | Dr::TglDpllDiv0(_)
            | Dr::Mg(
                _,
                MgRegister::RefclkInCtl
                | MgRegister::CoreClkCtl1
                | MgRegister::HsClkCtl
                | MgRegister::PllDiv0
                | MgRegister::PllDiv1
                | MgRegister::PllLf
                | MgRegister::PllFracLock
                | MgRegister::PllSsc
                | MgRegister::PllBias
                | MgRegister::PllTdcColdStartBias,
            )
            | Dr::TransCmtgChicken => None,
        };
        let _ = write;
        mapped
    }

    fn raw_read(&mut self, register: Register) -> u32 {
        if self.fault.is_some() {
            return u32::MAX;
        }
        if self.power.verify_access(self.registers).is_err() {
            self.fail(DpllFailure::DklAccessUnavailable);
            return u32::MAX;
        }
        match self.registers.read(register) {
            Some(value) if value != u32::MAX => value,
            _ => {
                self.fail(DpllFailure::RegisterUnavailable);
                u32::MAX
            }
        }
    }

    fn raw_write(&mut self, register: Register, value: u32) {
        if self.fault.is_some() {
            return;
        }
        if self.power.verify_access(self.registers).is_err() {
            self.fail(DpllFailure::DklAccessUnavailable);
            return;
        }
        if self.registers.write(register, value) {
            self.writes = self.writes.saturating_add(1);
        } else {
            self.fail(DpllFailure::RegisterUnavailable);
        }
    }

    fn alloc_power_ref(&mut self, domain: dpll::PowerDomain) -> Option<u64> {
        let Some(slot) = self.power_refs.iter().position(Option::is_none) else {
            self.fail(DpllFailure::PowerRefLedgerFull);
            return None;
        };
        let cookie = *self.next_cookie;
        let Some(next) = cookie.checked_add(1) else {
            self.fail(DpllFailure::PowerRefLedgerFull);
            return None;
        };
        if cookie == 0 {
            self.fail(DpllFailure::PowerRefLedgerFull);
            return None;
        }
        *self.next_cookie = next;
        self.power_refs[slot] = Some(PowerRef { cookie, domain });
        Some(cookie)
    }

    fn acquire_domain(&mut self, domain: dpll::PowerDomain) -> u64 {
        if self.fault.is_some() {
            return 0;
        }
        if let Err(error) = self.power.power_get(self.registers, domain) {
            self.fail(error);
            return 0;
        }
        match self.alloc_power_ref(domain) {
            Some(cookie) => cookie,
            None => {
                if self.power.power_put(self.registers, domain).is_err() {
                    self.fail(DpllFailure::PowerDomain);
                }
                0
            }
        }
    }

    fn tc_domains_held(&self, port: dpll::TcPort) -> bool {
        self.power.tc_port_held(self.registers, port)
    }

    fn dkl_register(
        &mut self,
        port: dpll::TcPort,
        offset: u16,
    ) -> Option<intel_display::dkl_phy::DklRegister> {
        if !matches!(port, dpll::TcPort::Tc1 | dpll::TcPort::Tc2) {
            self.fail(DpllFailure::UnsupportedRegister);
            return None;
        }
        if !self.tc_domains_held(port) {
            self.fail(DpllFailure::DklPortNotPowered);
            return None;
        }
        let dkl_port = match port {
            dpll::TcPort::Tc1 => intel_display::dkl_phy::TcPort::Tc1,
            dpll::TcPort::Tc2 => intel_display::dkl_phy::TcPort::Tc2,
            _ => unreachable!("TC port checked above"),
        };
        match intel_display::dkl_phy::DklRegister::new(dkl_port, u32::from(offset)) {
            Ok(register) => Some(register),
            Err(_) => {
                self.fail(DpllFailure::UnsupportedRegister);
                None
            }
        }
    }

    fn dkl_read(&mut self, port: dpll::TcPort, offset: u16) -> u32 {
        let Some(register) = self.dkl_register(port, offset) else {
            return u32::MAX;
        };
        let _hip = crate::drm::intel::DKL_ACCESS_LOCK.lock();
        self.raw_write(
            Register::read_write(
                "HIP_INDEX_REG0",
                register.selector(),
                Meaning::BringUp,
                None,
            ),
            register.index_value(),
        );
        if self.fault.is_some() {
            return u32::MAX;
        }
        self.raw_read(Register::read_only(
            "DKL_PHY_INDEXED",
            register.aperture(),
            Meaning::BringUp,
            None,
        ))
    }

    fn dkl_write(&mut self, port: dpll::TcPort, offset: u16, value: u32) {
        let Some(register) = self.dkl_register(port, offset) else {
            return;
        };
        let _hip = crate::drm::intel::DKL_ACCESS_LOCK.lock();
        self.raw_write(
            Register::read_write(
                "HIP_INDEX_REG0",
                register.selector(),
                Meaning::BringUp,
                None,
            ),
            register.index_value(),
        );
        if self.fault.is_none() {
            self.raw_write(
                Register::read_write(
                    "DKL_PHY_INDEXED",
                    register.aperture(),
                    Meaning::BringUp,
                    None,
                ),
                value,
            );
        }
    }

    fn dpll_register_read(&mut self, register: dpll::DpllRegister) -> Option<u32> {
        let Some(reg) = Self::register(register, false) else {
            self.fail(DpllFailure::UnsupportedRegister);
            return None;
        };
        Some(self.raw_read(reg))
    }

    fn dpll_register_write(&mut self, register: dpll::DpllRegister, value: u32) {
        let Some(reg) = Self::register(register, true) else {
            self.fail(DpllFailure::UnsupportedRegister);
            return;
        };
        self.raw_write(reg, value);
    }

    fn poll(
        &mut self,
        register: dpll::DpllRegister,
        mask: u32,
        set: bool,
        timeout_ms: u32,
    ) -> bool {
        let timeout_us = u64::from(timeout_ms).saturating_mul(1_000);
        let start = self.timer.now_micros();
        let max_polls = timeout_ms.max(1).saturating_mul(MAX_WAIT_POLLS_PER_MS);
        for _ in 0..max_polls {
            let value = dpll::IntelDpllHooks::read32(self, register);
            if self.fault.is_some() {
                return true;
            }
            if (value & mask != 0) == set {
                return false;
            }
            if self.timer.now_micros().saturating_sub(start) >= timeout_us {
                break;
            }
            self.timer.pause();
        }
        self.fail(DpllFailure::WaitTimeout);
        true
    }
}

impl<R: Registers, T: PollTimer, P: DpllPowerAccess<R>> dpll::IntelDpllHooks
    for KernelDpllHooks<'_, R, T, P>
{
    fn read32(&mut self, register: dpll::DpllRegister) -> u32 {
        self.dpll_register_read(register).unwrap_or(u32::MAX)
    }

    fn write32(&mut self, register: dpll::DpllRegister, value: u32) {
        self.dpll_register_write(register, value);
    }

    fn posting_read32(&mut self, register: dpll::DpllRegister) {
        let _ = self.read32(register);
    }

    fn read_dkl(&mut self, port: dpll::TcPort, offset: u16) -> u32 {
        self.dkl_read(port, offset)
    }

    fn write_dkl(&mut self, port: dpll::TcPort, offset: u16, value: u32) {
        self.dkl_write(port, offset, value);
    }

    fn posting_read_dkl(&mut self, port: dpll::TcPort, offset: u16) {
        let _ = self.dkl_read(port, offset);
    }

    fn wait_for_set(&mut self, register: dpll::DpllRegister, mask: u32, timeout_ms: u32) -> bool {
        self.poll(register, mask, true, timeout_ms)
    }

    fn wait_for_clear(&mut self, register: dpll::DpllRegister, mask: u32, timeout_ms: u32) -> bool {
        self.poll(register, mask, false, timeout_ms)
    }

    fn power_get(&mut self, _display_id: usize, domain: dpll::PowerDomain) -> u64 {
        self.acquire_domain(domain)
    }

    fn power_get_if_enabled(
        &mut self,
        _display_id: usize,
        domain: dpll::PowerDomain,
    ) -> Option<u64> {
        if !self.power_refs.iter().any(Option::is_none)
            || self.next_cookie.checked_add(1).is_none()
            || *self.next_cookie == 0
        {
            self.fail(DpllFailure::PowerRefLedgerFull);
            return None;
        }
        match self.power.power_get_if_enabled(self.registers, domain) {
            Ok(true) => match self.alloc_power_ref(domain) {
                Some(cookie) => Some(cookie),
                None => {
                    if self.power.power_put(self.registers, domain).is_err() {
                        self.fail(DpllFailure::PowerDomain);
                    }
                    None
                }
            },
            Ok(false) => None,
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn power_put(&mut self, _display_id: usize, domain: dpll::PowerDomain, cookie: u64) {
        let Some(index) = self.power_refs.iter().position(|reference| {
            reference
                .is_some_and(|reference| reference.cookie == cookie && reference.domain == domain)
        }) else {
            self.fail(DpllFailure::InvalidPowerRef);
            return;
        };
        if self.power.power_put(self.registers, domain).is_err() {
            self.fail(DpllFailure::PowerDomain);
            return;
        }
        self.power_refs[index] = None;
    }

    fn dpll_mutex_init(&mut self, _display_id: usize) {
        // The static spin mutex below is initialized at compile time.
    }

    fn dpll_mutex_lock(&mut self, _display_id: usize) {
        if self.dpll_guard().is_some() {
            self.fail(DpllFailure::UnexpectedState);
        } else {
            self.dpll_lock_slot = Some(DPLL_HW_LOCK.lock());
        }
    }

    fn dpll_mutex_unlock(&mut self, _display_id: usize) {
        if self.dpll_guard().is_none() {
            self.fail(DpllFailure::UnexpectedState);
        } else {
            self.dpll_lock_slot.take();
        }
    }

    fn connection_mutex_is_locked(&mut self, _display_id: usize) -> bool {
        self.connection_locked
    }

    fn drm_debug(&mut self, _display_id: usize, _message: &str) {
        self.debug_count = self.debug_count.saturating_add(1);
    }

    fn drm_error(&mut self, _display_id: usize, _message: &str) {
        self.fail(DpllFailure::UnexpectedState);
    }

    fn drm_warn(&mut self, _display_id: usize, _message: &str) {
        self.warning_count = self.warning_count.saturating_add(1);
        self.fail(DpllFailure::UnexpectedState);
    }

    fn drm_warn_on(&mut self, _display_id: usize, condition: bool, _message: &str) -> bool {
        if condition {
            self.warning_count = self.warning_count.saturating_add(1);
            self.fail(DpllFailure::UnexpectedState);
        }
        condition
    }

    fn display_state_warn(&mut self, _display_id: usize, condition: bool, _message: &str) -> bool {
        self.drm_warn_on(0, condition, "display state warning")
    }

    fn missing_case(&mut self, _display_id: usize, _value: u32) {
        self.fail(DpllFailure::UnsupportedIdentity);
    }

    fn hti_dpll_mask(&mut self, _display_id: usize) -> u32 {
        self.fail(DpllFailure::UnsupportedIdentity);
        0
    }

    fn is_adlp_step_a0_to_b0(&mut self, _display_id: usize) -> bool {
        // Verification admits only exact modeled N revisions; the current
        // N305 device table maps revision 0 to D0, never A0/B0.
        false
    }

    fn intel_cx0pll_verify_plls(&mut self, _display_id: usize) {
        // CX0 belongs to newer display IP and is not present on display 13.
    }

    fn intel_lt_phy_verify_plls(&mut self, _display_id: usize) {
        // Legacy LT PHY is not part of the ADL-N DPLL manager.
    }

    fn intel_cx0_pll_power_save_wa(&mut self, _display_id: usize) {
        // CX0 power-save is not applicable to the verified ADL-N device.
    }

    fn log_hw_state(&mut self, _display_id: usize, _title: &str, _state: &dpll::IntelDpllHwState) {
        self.debug_count = self.debug_count.saturating_add(1);
    }
}

// `dpll_guard` is separate from the DKL HIP selector guard: both are held only
// across their source-defined critical sections.
impl<R, T, P> KernelDpllHooks<'_, R, T, P> {
    fn dpll_guard(&self) -> Option<&MutexGuard<'static, ()>> {
        self.dpll_lock_slot.as_ref()
    }
}

fn tc_port_from_crtc(port: dpll::Port) -> Result<dpll::TcPort, DpllFailure> {
    match port {
        dpll::Port::Tc(dpll::TcPort::Tc1) | dpll::Port::C => Ok(dpll::TcPort::Tc1),
        dpll::Port::Tc(dpll::TcPort::Tc2) | dpll::Port::D => Ok(dpll::TcPort::Tc2),
        _ => Err(DpllFailure::UnsupportedIdentity),
    }
}

fn tc_domains_held_in_power_state(power: &PowerState, port: dpll::TcPort) -> bool {
    let (lanes, io) = match port {
        dpll::TcPort::Tc1 => (
            KernelPowerDomain::PortDdiLanesTc1,
            KernelPowerDomain::PortDdiIoTc1,
        ),
        dpll::TcPort::Tc2 => (
            KernelPowerDomain::PortDdiLanesTc2,
            KernelPowerDomain::PortDdiIoTc2,
        ),
        _ => return false,
    };
    tc_domain_counts_held(
        port,
        power.power_domains.domain_use_count(lanes),
        power.power_domains.domain_use_count(io),
    )
}

fn tc_domain_counts_held(port: dpll::TcPort, lane_refs: u32, io_refs: u32) -> bool {
    matches!(port, dpll::TcPort::Tc1 | dpll::TcPort::Tc2) && lane_refs > 0 && io_refs > 0
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;

    use super::*;

    struct NoRegisters;
    impl Registers for NoRegisters {
        fn read(&self, _register: Register) -> Option<u32> {
            None
        }
        fn read64(&self, _register: Register) -> Option<u64> {
            None
        }
        fn write(&self, _register: Register, _value: u32) -> bool {
            false
        }
    }

    struct NoTime;
    impl PollTimer for NoTime {
        fn now_micros(&self) -> u64 {
            0
        }
        fn pause(&self) {}
    }

    struct PinRegisters {
        words: spin::Mutex<BTreeMap<u32, u32>>,
        writes: core::sync::atomic::AtomicUsize,
    }

    impl PinRegisters {
        fn new() -> Self {
            Self {
                words: spin::Mutex::new(BTreeMap::from([
                    (0x45404, 0x405),
                    (0x45454, 0x40),
                    (0x45444, 0x40),
                    (0x45504, 0),
                    (0x51004, 0),
                ])),
                writes: core::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn set(&self, offset: u32, value: u32) {
            self.words.lock().insert(offset, value);
        }

        fn write_count(&self) -> usize {
            self.writes.load(core::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Registers for PinRegisters {
        fn read(&self, register: Register) -> Option<u32> {
            self.words.lock().get(&register.offset()).copied()
        }

        fn read64(&self, _register: Register) -> Option<u64> {
            None
        }

        fn write(&self, register: Register, value: u32) -> bool {
            let offset = register.offset();
            let mut words = self.words.lock();
            let old = words.get(&offset).copied().unwrap_or(0);
            let payload = if matches!(offset, 0x45404 | 0x45454 | 0x45444) {
                (value & 0xaaaa_aaaa) | (old & 0x5555_5555)
            } else {
                value
            };
            words.insert(offset, payload);
            self.writes
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            true
        }
    }

    struct FakePower;
    impl DpllPowerAccess<NoRegisters> for FakePower {
        fn reference_clock_khz(&self, _registers: &NoRegisters) -> Result<u32, DpllFailure> {
            Ok(24_000)
        }
        fn begin_access(
            &mut self,
            _registers: &NoRegisters,
            hold_display_core: bool,
        ) -> Result<bool, DpllFailure> {
            Ok(hold_display_core)
        }
        fn end_access(
            &mut self,
            _registers: &NoRegisters,
            _core_cookie: bool,
        ) -> Result<(), DpllFailure> {
            Ok(())
        }
        fn power_get(
            &mut self,
            _registers: &NoRegisters,
            _domain: dpll::PowerDomain,
        ) -> Result<(), DpllFailure> {
            Ok(())
        }
        fn power_get_if_enabled(
            &mut self,
            _registers: &NoRegisters,
            _domain: dpll::PowerDomain,
        ) -> Result<bool, DpllFailure> {
            Ok(true)
        }
        fn power_put(
            &mut self,
            _registers: &NoRegisters,
            _domain: dpll::PowerDomain,
        ) -> Result<(), DpllFailure> {
            Ok(())
        }
        fn tc_port_held(&self, _registers: &NoRegisters, port: dpll::TcPort) -> bool {
            matches!(port, dpll::TcPort::Tc1 | dpll::TcPort::Tc2)
        }
        fn verify_access(&self, _registers: &NoRegisters) -> Result<(), DpllFailure> {
            Ok(())
        }
    }

    #[test]
    fn dpll_identity_admission_requires_known_adln_revision() {
        let identity = AdlNIdentity::verify(0x8086, 0x46d0, 0).expect("ADL-N rev 0 is modeled");
        assert_eq!(identity.stepping, DisplayStepping::D0);
        assert_eq!(
            AdlNIdentity::verify(0x8086, 0x46d0, 1),
            Err(DpllFailure::UnknownStepping)
        );
        assert_eq!(
            AdlNIdentity::verify(0x8086, 0x46d5, 0),
            Err(DpllFailure::UnsupportedIdentity)
        );
        assert_eq!(
            AdlNIdentity::verify(0x1234, 0x46d0, 0),
            Err(DpllFailure::UnsupportedIdentity)
        );
    }

    #[test]
    fn tc_dpll_scope_hides_the_unpowered_sibling_phy() {
        let identity = AdlNIdentity::verify(0x8086, 0x46d0, 0).unwrap();
        let mut tc1 = SharedDpllState::new(identity, 0);
        tc1.initialized = true;
        tc1.display.num_dpll = 5;
        assert_eq!(
            tc1.limit_to_tc_port(intel_display::dkl_phy::TcPort::Tc1),
            Ok(3)
        );
        assert_eq!(tc1.display.num_dpll, 4);

        let mut tc2 = SharedDpllState::new(identity, 0);
        tc2.initialized = true;
        tc2.display.num_dpll = 5;
        tc2.display.dplls[3].info = Some(dpll::DpllInfo {
            name: "TC PLL 1",
            funcs: dpll::DpllFunction::Dkl,
            id: dpll::DPLL_ID_ICL_MGPLL1,
            power_domain: None,
            always_on: false,
            is_alt_port_dpll: false,
        });
        tc2.display.dplls[4].info = Some(dpll::DpllInfo {
            name: "TC PLL 2",
            funcs: dpll::DpllFunction::Dkl,
            id: dpll::DPLL_ID_ICL_MGPLL2,
            power_domain: None,
            always_on: false,
            is_alt_port_dpll: false,
        });
        assert_eq!(
            tc2.limit_to_tc_port(intel_display::dkl_phy::TcPort::Tc2),
            Ok(3)
        );
        assert_eq!(tc2.display.num_dpll, 4);
        assert_eq!(
            tc2.display.dplls[3].info.map(|info| info.id),
            Some(dpll::DPLL_ID_ICL_MGPLL2)
        );
        assert_eq!(tc2.display.dplls[3].index, 3);
        assert!(tc2.display.dplls[4].info.is_some());
    }

    #[test]
    fn selected_dkl_readout_uses_source_comparator_for_firmware_fields() {
        let firmware = intel_display::dpll_mgr::DklPllState {
            dco_khz: 8_100_000,
            refclkin_ctl: 1 << 8,
            coreclkctl1: 3 << 8,
            hsclkctl: (1 << 16) | (2 << 14) | (1 << 12) | (5 << 8),
            div0: (4 << 16) | (5 << 12) | (2 << 8) | 0x7c,
            div1: (25 << 16) | 0x26,
            ssc: (1 << 29) | (9 << 16) | (3 << 11) | (1 << 9),
            bias: (1 << 30) | (0x1234 << 8),
            tdc_coldst_bias: 0x3456,
        };
        let mut dispatched = dpll::IntelDpllHwState::default();
        dispatched.icl.mg_refclkin_ctl = firmware.refclkin_ctl;
        dispatched.icl.mg_clktop2_coreclkctl1 = firmware.coreclkctl1;
        dispatched.icl.mg_clktop2_hsclkctl = firmware.hsclkctl;
        dispatched.icl.mg_pll_div0 = firmware.div0 | (1 << 24);
        dispatched.icl.mg_pll_div1 = firmware.div1 | (1 << 31);
        dispatched.icl.mg_pll_ssc = firmware.ssc | (1 << 5);
        dispatched.icl.mg_pll_bias_mask = u32::MAX;
        dispatched.icl.mg_pll_bias = firmware.bias | 1;
        dispatched.icl.mg_pll_tdc_coldst_bias_mask = u32::MAX;
        dispatched.icl.mg_pll_tdc_coldst_bias = firmware.tdc_coldst_bias | (1 << 20);
        assert!(dkl_state_matches_source_readout(
            &firmware,
            &dispatched,
            false
        ));

        let mut mismatched = dispatched;
        mismatched.icl.mg_pll_div1 ^= 1;
        assert!(!dkl_state_matches_source_readout(
            &firmware,
            &mismatched,
            false
        ));
    }

    #[test]
    fn dpll_register_whitelist_covers_combo_tbt_and_tc12() {
        use dpll::{CfgRegisterFamily::TigerLake, DpllId, DpllRegister as Dr, TcPort};
        type Hooks = KernelDpllHooks<'static, NoRegisters, NoTime, FakePower>;

        assert_eq!(
            Hooks::register(
                Dr::ComboEnable(dpll::ComboEnableFamily::Icl, DpllId(0)),
                true
            ),
            Some(regs::dpll::DPLL0_ENABLE)
        );
        assert_eq!(
            Hooks::register(Dr::CfgCr1(TigerLake, DpllId(1)), true),
            Some(regs::dpll::DPLL1_CFGCR1)
        );
        assert_eq!(
            Hooks::register(Dr::AdlpTcEnable(TcPort::Tc1), true),
            Some(PORTTC1_PLL_ENABLE)
        );
        assert_eq!(
            Hooks::register(Dr::AdlpTcEnable(TcPort::Tc2), true),
            Some(PORTTC2_PLL_ENABLE)
        );
        assert_eq!(
            Hooks::register(Dr::AdlpTcEnable(TcPort::Tc3), true),
            None,
            "TC3 lacks an N305-verified typed enable register"
        );
        assert_eq!(
            Hooks::register(Dr::TglDpllDiv0(DpllId(0)), true),
            None,
            "AFC override is not admitted without VBT evidence"
        );
    }

    #[test]
    fn dpll_active_tc_route_mapping_rejects_unowned_ports() {
        assert_eq!(tc_port_from_crtc(dpll::Port::C), Ok(dpll::TcPort::Tc1));
        assert_eq!(tc_port_from_crtc(dpll::Port::D), Ok(dpll::TcPort::Tc2));
        assert_eq!(
            tc_port_from_crtc(dpll::Port::Tc(dpll::TcPort::Tc3)),
            Err(DpllFailure::UnsupportedIdentity)
        );
        assert_eq!(
            tc_port_from_crtc(dpll::Port::F),
            Err(DpllFailure::UnsupportedIdentity)
        );
    }

    #[test]
    fn dpll_tc_phy_access_requires_lane_and_io_references() {
        for port in [dpll::TcPort::Tc1, dpll::TcPort::Tc2] {
            assert!(!tc_domain_counts_held(port, 0, 0));
            assert!(!tc_domain_counts_held(port, 1, 0));
            assert!(!tc_domain_counts_held(port, 0, 1));
            assert!(tc_domain_counts_held(port, 1, 1));
        }
        assert!(!tc_domain_counts_held(dpll::TcPort::Tc3, 1, 1));
    }

    #[test]
    fn dpll_power_map_admission_requires_exact_tc1_tc2_dependencies() {
        assert_eq!(verify_adln_pin_map(dpll::TcPort::Tc1), Ok(()));
        assert_eq!(verify_adln_pin_map(dpll::TcPort::Tc2), Ok(()));
        assert_eq!(
            verify_adln_pin_map(dpll::TcPort::Tc3),
            Err(DpllFailure::UnsupportedIdentity)
        );
    }

    #[test]
    fn dpll_logical_power_cookie_is_balanced_and_rejects_double_put() {
        let registers = NoRegisters;
        let timer = NoTime;
        let mut power = FakePower;
        let mut refs = [None; POWER_REF_SLOTS];
        let mut next_cookie = 1;
        let mut hooks = KernelDpllHooks {
            registers: &registers,
            timer: &timer,
            power: &mut power,
            power_refs: &mut refs,
            next_cookie: &mut next_cookie,
            fault: None,
            debug_count: 0,
            warning_count: 0,
            writes: 0,
            connection_locked: true,
            dpll_lock_slot: None,
        };
        let cookie = dpll::IntelDpllHooks::power_get_if_enabled(
            &mut hooks,
            1,
            dpll::PowerDomain::DisplayCore,
        )
        .expect("active backend grants a logical core cookie");
        assert_eq!(hooks.power_refs.iter().flatten().count(), 1);
        dpll::IntelDpllHooks::power_put(&mut hooks, 1, dpll::PowerDomain::DisplayCore, cookie);
        assert_eq!(hooks.power_refs.iter().flatten().count(), 0);
        dpll::IntelDpllHooks::power_put(&mut hooks, 1, dpll::PowerDomain::DisplayCore, cookie);
        assert_eq!(hooks.fault, Some(DpllFailure::InvalidPowerRef));
    }

    #[test]
    fn dpll_pinned_power_backend_is_read_only_and_cookie_balanced() {
        let registers = PinRegisters::new();
        let pin = crate::drm::intel::fastboot::PowerPin::acquire(
            &registers,
            intel_display::dkl_phy::TcPort::Tc1,
        )
        .unwrap();
        let identity = AdlNIdentity::verify(0x8086, 0x46d0, 0).unwrap();
        let mut power = PinnedDpllPower::new(&pin, identity, 24_000).unwrap();
        let timer = NoTime;
        let writes_before_use = registers.write_count();
        let mut refs = [None; POWER_REF_SLOTS];
        let mut next_cookie = 1;
        let mut hooks = KernelDpllHooks {
            registers: &registers,
            timer: &timer,
            power: &mut power,
            power_refs: &mut refs,
            next_cookie: &mut next_cookie,
            fault: None,
            debug_count: 0,
            warning_count: 0,
            writes: 0,
            connection_locked: true,
            dpll_lock_slot: None,
        };
        let cookie = dpll::IntelDpllHooks::power_get_if_enabled(
            &mut hooks,
            1,
            dpll::PowerDomain::DisplayCore,
        )
        .expect("held pin yields only a logical DPLL cookie");
        assert_eq!(
            dpll::IntelDpllHooks::power_get_if_enabled(
                &mut hooks,
                1,
                dpll::PowerDomain::DisplayCore,
            ),
            Some(cookie + 1)
        );
        assert_eq!(registers.write_count(), writes_before_use);
        dpll::IntelDpllHooks::power_put(&mut hooks, 1, dpll::PowerDomain::DisplayCore, cookie);
        assert_eq!(hooks.power_refs.iter().flatten().count(), 1);
        dpll::IntelDpllHooks::power_put(&mut hooks, 1, dpll::PowerDomain::DisplayCore, cookie + 1);
        assert_eq!(hooks.power_refs.iter().flatten().count(), 0);

        registers.set(0x45454, 0x40); // clear the TC1 DDI-IO request bit
        assert_eq!(
            dpll::IntelDpllHooks::power_get_if_enabled(
                &mut hooks,
                1,
                dpll::PowerDomain::DisplayCore,
            ),
            None
        );
        assert_eq!(hooks.fault, Some(DpllFailure::DklAccessUnavailable));
        assert_eq!(registers.write_count(), writes_before_use);
    }
}

// A read-only all-ones return is treated as unavailable by the adapter.  This
// type-level helper is intentionally not a success-default: all unsupported
// family/ID pairs route through `DpllFailure::UnsupportedRegister`.
