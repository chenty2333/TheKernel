// SPDX-License-Identifier: MIT
//! Kernel-side adapter for the translated Gen12/13 shared-DPLL manager.
//!
//! This module intentionally admits only the modeled Alder Lake-N display-13
//! family.  It exposes the translated reservation/readout and generic
//! `intel_dpll_enable()` / `intel_dpll_disable()` lifecycle behind the kernel's
//! typed MMIO, DKL selector lock, and refcounted display power state.  The
//! caller remains responsible for the larger atomic commit and its complete
//! modeset rollback image.

use intel_display::{intel_dpll_mgr_full as dpll, power_map::PowerDomain as KernelPowerDomain};
use spin::{Mutex, MutexGuard};

use super::{
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
    domain: KernelPowerDomain,
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
/// checkpoint.  This restores manager masks only after the outer modeset layer
/// has proved its MMIO before-image rollback; it is not a substitute for it.
pub(crate) struct SharedDpllUndo {
    display: dpll::IntelDpllDisplay,
    power_counts: [u32; 6],
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
            display_core_refs: refs_for(KernelPowerDomain::DisplayCore),
            dc_off_refs: refs_for(KernelPowerDomain::DcOff),
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

    fn refresh_reference_clock(&mut self, power: &PowerState) -> Result<(), DpllFailure> {
        let observation = power.cdclk.observed;
        if !observation.reference_recognised {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        let reference_khz = observation.reference.khz();
        if !matches!(reference_khz, 19_200 | 24_000 | 38_400) {
            return Err(DpllFailure::UnsupportedIdentity);
        }
        self.display.cdclk_ref = reference_khz;
        dpll::intel_dpll_update_ref_clks(&mut self.display);
        Ok(())
    }

    /// Initialize the translated ADLP manager.  N305 uses the source-verified
    /// combo PLLs, TBT PLL, and DKL TC1/TC2 PLLs; manager entries for TC3/TC4
    /// are excluded because their N305 enable-register mapping is not proven.
    pub(crate) fn init<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
    ) -> Result<(), DpllFailure> {
        if self.initialized {
            return Err(DpllFailure::AlreadyInitialized);
        }
        self.refresh_reference_clock(power)?;
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

    /// Read hardware state once from caller-proved CRTC readouts.  The source
    /// manager dispatches DKL TC1/TC2 through `intel_dpll_get_hw_state()` to
    /// its internal `dkl_pll_get_hw_state()` implementation.
    pub(crate) fn readout<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        crtcs: [dpll::CrtcState; dpll::MAX_PIPES],
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        if self.readout_done {
            return Err(DpllFailure::AlreadyInitialized);
        }
        self.refresh_reference_clock(power)?;
        if crtcs.iter().any(|crtc| crtc.hw_active)
            && !power
                .is_domain_enabled(registers, KernelPowerDomain::DisplayCore, true)
                .unwrap_or(false)
        {
            return Err(DpllFailure::PowerDomain);
        }
        for crtc in crtcs.iter().filter(|crtc| crtc.hw_active) {
            self.check_crtc_pll_power_domains(power, crtc)?;
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
    pub(crate) fn get_hw_state<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        pll_index: usize,
    ) -> Result<(bool, dpll::IntelDpllHwState), DpllFailure> {
        self.ensure_usable()?;
        if pll_index >= self.display.num_dpll {
            return Err(DpllFailure::InvalidCrtc);
        }
        self.refresh_reference_clock(power)?;
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
    pub(crate) fn sanitize<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        if !self.readout_done {
            return Err(DpllFailure::Uninitialized);
        }
        self.refresh_reference_clock(power)?;
        self.with_hooks(registers, timer, power, true, true, |hooks, display| {
            dpll::intel_dpll_sanitize_state(hooks, display);
        })
    }

    pub(crate) fn compute<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        atomic: &mut dpll::IntelAtomicState,
        crtc: &dpll::IntelCrtc,
        encoder: &dpll::IntelEncoder,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(power)?;
        self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            let error = dpll::intel_dpll_compute(hooks, display, atomic, crtc, encoder);
            if error != 0 {
                hooks.fail(DpllFailure::ManagerRejected(error));
            }
        })
    }

    pub(crate) fn reserve<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        atomic: &mut dpll::IntelAtomicState,
        crtc: &dpll::IntelCrtc,
        encoder: &dpll::IntelEncoder,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(power)?;
        self.with_hooks(registers, timer, power, false, false, |hooks, display| {
            let error = dpll::intel_dpll_reserve(hooks, display, atomic, crtc, encoder);
            if error != 0 {
                hooks.fail(DpllFailure::ManagerRejected(error));
            }
        })
    }

    pub(crate) fn release<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
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
        power: &PowerState,
    ) -> Result<SharedDpllUndo, DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        let _connection = DPLL_CONNECTION_LOCK.lock();
        if self.power_refs.iter().any(Option::is_some) {
            return Err(DpllFailure::InvalidPowerRef);
        }
        let undo = SharedDpllUndo {
            display: self.display.clone(),
            power_counts: [
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::DisplayCore),
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::DcOff),
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::PortDdiLanesTc1),
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::PortDdiIoTc1),
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::PortDdiLanesTc2),
                power
                    .power_domains
                    .domain_use_count(KernelPowerDomain::PortDdiIoTc2),
            ],
        };
        dpll::intel_dpll_swap_state(&mut self.display, atomic);
        Ok(undo)
    }

    /// Restore manager/refcount state only after the caller's modeset
    /// checkpoint has fully restored and verified the prior hardware image.
    /// A leaked translation wakeref or changed source-domain count keeps this
    /// manager quarantined rather than fabricating a balanced rollback.
    pub(crate) fn restore_after_verified_hardware_rollback(
        &mut self,
        power: &PowerState,
        undo: SharedDpllUndo,
    ) -> Result<(), DpllFailure> {
        let _connection = DPLL_CONNECTION_LOCK.lock();
        let current_counts = [
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::DisplayCore),
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::DcOff),
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::PortDdiLanesTc1),
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::PortDdiIoTc1),
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::PortDdiLanesTc2),
            power
                .power_domains
                .domain_use_count(KernelPowerDomain::PortDdiIoTc2),
        ];
        if self.power_refs.iter().any(Option::is_some) || current_counts != undo.power_counts {
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
    pub(crate) fn enable<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        crtc_state: &dpll::CrtcState,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(power)?;
        self.check_crtc_pll_power_domains(power, crtc_state)?;
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
    pub(crate) fn disable<R: Registers, T: PollTimer>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        crtc_state: &dpll::CrtcState,
    ) -> Result<(), DpllFailure> {
        self.ensure_usable()?;
        self.ensure_readout()?;
        self.refresh_reference_clock(power)?;
        self.check_crtc_pll_power_domains(power, crtc_state)?;
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

    fn check_crtc_pll_power_domains(
        &self,
        power: &PowerState,
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
                if tc_domains_held(power, dpll::TcPort::Tc1) {
                    Ok(())
                } else {
                    Err(DpllFailure::DklPortNotPowered)
                }
            }
            (dpll::Port::D | dpll::Port::Tc(dpll::TcPort::Tc2), dpll::DpllFunction::Dkl, 4) => {
                if tc_domains_held(power, dpll::TcPort::Tc2) {
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

    fn with_hooks<R, T, O>(
        &mut self,
        registers: &R,
        timer: &T,
        power: &mut PowerState,
        hold_display_core: bool,
        quarantine_on_fault: bool,
        operation: impl FnOnce(&mut KernelDpllHooks<'_, R, T>, &mut dpll::IntelDpllDisplay) -> O,
    ) -> Result<O, DpllFailure>
    where
        R: Registers,
        T: PollTimer,
    {
        if self.quarantined {
            return Err(DpllFailure::Quarantined);
        }
        let _connection = DPLL_CONNECTION_LOCK.lock();
        let got_core = if hold_display_core {
            power
                .get_domain(registers, KernelPowerDomain::DisplayCore)
                .map_err(|_| DpllFailure::PowerDomain)?;
            true
        } else {
            false
        };
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

        let release_error = if got_core {
            power
                .put_domain(registers, KernelPowerDomain::DisplayCore)
                .err()
        } else {
            None
        };
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

struct KernelDpllHooks<'a, R, T> {
    registers: &'a R,
    timer: &'a T,
    power: &'a mut PowerState,
    power_refs: &'a mut [Option<PowerRef>; POWER_REF_SLOTS],
    next_cookie: &'a mut u64,
    fault: Option<DpllFailure>,
    debug_count: u32,
    warning_count: u32,
    writes: u32,
    connection_locked: bool,
    dpll_lock_slot: Option<MutexGuard<'static, ()>>,
}

impl<R: Registers, T: PollTimer> KernelDpllHooks<'_, R, T> {
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
        if self.registers.write(register, value) {
            self.writes = self.writes.saturating_add(1);
        } else {
            self.fail(DpllFailure::RegisterUnavailable);
        }
    }

    fn power_domain(domain: dpll::PowerDomain) -> KernelPowerDomain {
        match domain {
            dpll::PowerDomain::DisplayCore => KernelPowerDomain::DisplayCore,
            dpll::PowerDomain::DcOff => KernelPowerDomain::DcOff,
        }
    }

    fn alloc_power_ref(&mut self, domain: KernelPowerDomain) -> Option<u64> {
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

    fn acquire_domain(&mut self, domain: KernelPowerDomain) -> u64 {
        if self.fault.is_some() {
            return 0;
        }
        if self.power.get_domain(self.registers, domain).is_err() {
            self.fail(DpllFailure::PowerDomain);
            return 0;
        }
        match self.alloc_power_ref(domain) {
            Some(cookie) => cookie,
            None => {
                if self.power.put_domain(self.registers, domain).is_err() {
                    self.fail(DpllFailure::PowerDomain);
                }
                0
            }
        }
    }

    fn tc_domains_held(&self, port: dpll::TcPort) -> bool {
        tc_domains_held(self.power, port)
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
        let _hip = super::DKL_ACCESS_LOCK.lock();
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
        let _hip = super::DKL_ACCESS_LOCK.lock();
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

impl<R: Registers, T: PollTimer> dpll::IntelDpllHooks for KernelDpllHooks<'_, R, T> {
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
        self.acquire_domain(Self::power_domain(domain))
    }

    fn power_get_if_enabled(
        &mut self,
        _display_id: usize,
        domain: dpll::PowerDomain,
    ) -> Option<u64> {
        let domain = Self::power_domain(domain);
        if !self.power_refs.iter().any(Option::is_none)
            || self.next_cookie.checked_add(1).is_none()
            || *self.next_cookie == 0
        {
            self.fail(DpllFailure::PowerRefLedgerFull);
            return None;
        }
        match self
            .power
            .get_domain_if_enabled(self.registers, domain, true)
        {
            Ok(true) => match self.alloc_power_ref(domain) {
                Some(cookie) => Some(cookie),
                None => {
                    if self.power.put_domain(self.registers, domain).is_err() {
                        self.fail(DpllFailure::PowerDomain);
                    }
                    None
                }
            },
            Ok(false) => None,
            Err(_) => {
                self.fail(DpllFailure::PowerDomain);
                None
            }
        }
    }

    fn power_put(&mut self, _display_id: usize, domain: dpll::PowerDomain, cookie: u64) {
        let domain = Self::power_domain(domain);
        let Some(index) = self.power_refs.iter().position(|reference| {
            reference
                .is_some_and(|reference| reference.cookie == cookie && reference.domain == domain)
        }) else {
            self.fail(DpllFailure::InvalidPowerRef);
            return;
        };
        if self.power.put_domain(self.registers, domain).is_err() {
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
impl<R, T> KernelDpllHooks<'_, R, T> {
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

fn tc_domains_held(power: &PowerState, port: dpll::TcPort) -> bool {
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

    #[test]
    fn identity_admission_requires_known_adln_revision() {
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
    fn register_whitelist_covers_only_source_mapped_combo_tbt_and_tc12() {
        use dpll::{CfgRegisterFamily::TigerLake, DpllId, DpllRegister as Dr, TcPort};
        type Hooks = KernelDpllHooks<'static, NoRegisters, NoTime>;

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
    fn active_tc_route_mapping_rejects_unowned_ports() {
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
    fn tc_phy_access_requires_both_lane_and_io_references() {
        for port in [dpll::TcPort::Tc1, dpll::TcPort::Tc2] {
            assert!(!tc_domain_counts_held(port, 0, 0));
            assert!(!tc_domain_counts_held(port, 1, 0));
            assert!(!tc_domain_counts_held(port, 0, 1));
            assert!(tc_domain_counts_held(port, 1, 1));
        }
        assert!(!tc_domain_counts_held(dpll::TcPort::Tc3, 1, 1));
    }
}

// A read-only all-ones return is treated as unavailable by the adapter.  This
// type-level helper is intentionally not a success-default: all unsupported
// family/ID pairs route through `DpllFailure::UnsupportedRegister`.
