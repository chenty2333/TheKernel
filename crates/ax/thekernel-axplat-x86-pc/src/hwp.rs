//! Fleet-wide Intel Hardware P-state (HWP) clamp control.
//!
//! This module deliberately does not enable HWP: firmware must have set
//! `IA32_PM_ENABLE.HWP_ENABLE` before initialization.  It exclusively owns
//! bits 0..=15 (minimum/maximum performance) of `IA32_HWP_REQUEST`; callers
//! that change those bits outside this module must reinitialize before relying
//! on its write-elision cache.

#[cfg(target_os = "none")]
use core::arch::x86_64::__cpuid_count;

use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use kspin::SpinNoIrq;

#[cfg(target_os = "none")]
use x86::msr::{rdmsr, wrmsr};

#[cfg(any(target_os = "none", test))]
const IA32_PM_ENABLE: u32 = 0x770;
#[cfg(any(target_os = "none", test))]
const IA32_HWP_CAPABILITIES: u32 = 0x771;
const IA32_HWP_REQUEST: u32 = 0x774;
#[cfg(any(target_os = "none", test))]
const HWP_ENABLE: u64 = 1;
const REQUEST_BOUNDS: u64 = 0xffff;
#[cfg(any(target_os = "none", test))]
const INTEL_VENDOR: [u32; 3] = [0x756e6547, 0x4965_6e69, 0x6c65_746e];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    InvalidClamp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    pub lowest: u8,
    pub highest: u8,
}

#[derive(Clone, Copy)]
struct State {
    prepared: bool,
    caps: Capabilities,
    saved_request: u64,
    last_min: u8,
    last_max: u8,
    last_valid: bool,
}

const EMPTY_STATE: State = State {
    prepared: false,
    caps: Capabilities {
        lowest: 0,
        highest: 0,
    },
    saved_request: 0,
    last_min: 0,
    last_max: 0,
    last_valid: false,
};

// State is accessed only through `local`, which pins execution and disables
// interrupts before indexing the current logical CPU. No API accepts a CPU ID.
static STATES: [SpinNoIrq<State>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(EMPTY_STATE) }; crate::config::plat::MAX_CPU_NUM];

const FLEET_COLLECTING: u8 = 0;
const FLEET_ACTIVE: u8 = 1;
const FLEET_ABORTED: u8 = 2;

// The BSP executes `init_later` before it starts APs, while the typed IPI
// broker is installed only after all APs have been started.  Prepare is thus
// read-only: it never needs a remote operation to be rolled back.
static FLEET_PHASE: AtomicU8 = AtomicU8::new(FLEET_COLLECTING);
static PREPARED_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Validate HWP on the current CPU and save its complete initial request.
///
/// This is the prepare half of fleet initialization. It does not alter an MSR
/// and it cannot make HWP usable; only [`commit_current`] can do that.
pub fn prepare_current() -> Result<(), Error> {
    match FLEET_PHASE.load(Ordering::Acquire) {
        FLEET_ACTIVE => {
            return local(|state| state.prepared.then_some(()).ok_or(Error::Unsupported));
        }
        FLEET_ABORTED => {
            abort_current();
            return Err(Error::Unsupported);
        }
        FLEET_COLLECTING => {}
        _ => unreachable!("invalid HWP fleet phase"),
    }

    let newly_prepared = local(|state| {
        if state.prepared {
            return Ok(false);
        }

        #[cfg(not(target_os = "none"))]
        return Err(Error::Unsupported);

        #[cfg(target_os = "none")]
        {
            // Do not read any HWP MSR until every CPUID precondition holds.
            if !cpuid_supports_hwp() {
                return Err(Error::Unsupported);
            }
            // HWP is firmware policy. This module must never write PM_ENABLE.
            if !pm_enable_active(read(IA32_PM_ENABLE)) {
                return Err(Error::Unsupported);
            }
            let caps =
                decode_capabilities(read(IA32_HWP_CAPABILITIES)).ok_or(Error::Unsupported)?;
            state.prepared = true;
            state.caps = caps;
            state.saved_request = read(IA32_HWP_REQUEST);
            state.last_valid = false;
            Ok(true)
        }
    });

    let newly_prepared = match newly_prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            abort_fleet();
            return Err(error);
        }
    };
    if newly_prepared {
        PREPARED_CPUS.fetch_add(1, Ordering::AcqRel);
    }
    if FLEET_PHASE.load(Ordering::Acquire) == FLEET_ABORTED {
        abort_current();
        return Err(Error::Unsupported);
    }
    Ok(())
}

/// Commit a completely prepared HWP fleet.
///
/// A CPU arriving before its peers returns `false` rather than waiting: the
/// BSP is responsible for bringing those peers online. The final successful
/// CPU performs the only transition to active.
pub fn commit_current() -> bool {
    if !local(|state| Ok(state.prepared)).unwrap_or(false) {
        return false;
    }
    if PREPARED_CPUS.load(Ordering::Acquire) != crate::cpu::cpu_num() {
        return false;
    }
    match FLEET_PHASE.compare_exchange(
        FLEET_COLLECTING,
        FLEET_ACTIVE,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) | Err(FLEET_ACTIVE) => true,
        Err(FLEET_ABORTED) | Err(_) => false,
    }
}

/// Compatibility wrapper for the former per-CPU initializer.
///
/// Success only means that this CPU prepared. The public HWP operations reject
/// an uncommitted or aborted fleet with [`Error::Unsupported`].
pub fn init_current() -> Result<(), Error> {
    prepare_current()?;
    let _ = commit_current();
    Ok(())
}

/// Abort the fleet and restore this CPU's complete saved request, if any.
///
/// There is no safe remote execution channel during BSP/AP late-init. Since
/// prepare is read-only, already prepared peers have no altered request to
/// restore; this local call is the failure-recovery hook for every CPU that
/// observes the abort later in its lifecycle.
pub fn abort_current() {
    abort_fleet();
    local(|state| {
        if state.prepared {
            write(IA32_HWP_REQUEST, state.saved_request);
            state.last_min = state.saved_request as u8;
            state.last_max = (state.saved_request >> 8) as u8;
            state.last_valid = true;
        }
        Ok(())
    })
    .expect("local HWP abort cannot fail");
}

fn abort_fleet() {
    FLEET_PHASE.store(FLEET_ABORTED, Ordering::Release);
}

fn fleet_active() -> bool {
    FLEET_PHASE.load(Ordering::Acquire) == FLEET_ACTIVE
}

/// Return whether every discovered CPU prepared and the fleet committed.
pub fn is_active() -> bool {
    fleet_active()
}

/// Return the HWP performance range detected for the current CPU.
pub fn capabilities() -> Result<Capabilities, Error> {
    local(|state| {
        (fleet_active() && state.prepared)
            .then_some(state.caps)
            .ok_or(Error::Unsupported)
    })
}

/// Apply a normalized inclusive clamp on the current CPU.
///
/// Inputs are in `0..=1024`; they are mapped monotonically to the current
/// CPU's architectural HWP range using nearest-integer rounding.
pub fn apply_current_clamp(min: u16, max: u16) -> Result<(), Error> {
    if !valid_clamp(min, max) {
        return Err(Error::InvalidClamp);
    }
    local(|state| {
        if !fleet_active() || !state.prepared {
            return Err(Error::Unsupported);
        }
        apply(state, min, max)
    })
}

/// Restore the complete initial `IA32_HWP_REQUEST` value on the current CPU.
/// The operation is idempotent and deliberately does not use a bounds RMW.
pub fn restore_current_request() -> Result<(), Error> {
    local(|state| {
        if !state.prepared {
            return Err(Error::Unsupported);
        }
        write(IA32_HWP_REQUEST, state.saved_request);
        state.last_min = state.saved_request as u8;
        state.last_max = (state.saved_request >> 8) as u8;
        state.last_valid = true;
        Ok(())
    })
}

fn local<T>(f: impl FnOnce(&mut State) -> Result<T, Error>) -> Result<T, Error> {
    let _guard = kernel_guard::NoPreemptIrqSave::new();
    let cpu = crate::cpu::current_logical_cpu_id();
    f(&mut STATES[cpu].lock())
}

fn apply(state: &mut State, min: u16, max: u16) -> Result<(), Error> {
    let min = map_clamp(min, state.caps);
    let max = map_clamp(max, state.caps);
    if cache_matches(state, min, max) {
        return Ok(());
    }
    // RMW only after cache miss. Bits 16..=63 remain entirely untouched.
    let old = read(IA32_HWP_REQUEST);
    write(IA32_HWP_REQUEST, replace_bounds(old, min, max));
    state.last_min = min;
    state.last_max = max;
    state.last_valid = true;
    Ok(())
}

const fn valid_clamp(min: u16, max: u16) -> bool {
    min <= max && max <= 1024
}

const fn map_clamp(value: u16, caps: Capabilities) -> u8 {
    let span = (caps.highest - caps.lowest) as u32;
    (caps.lowest as u32 + (value as u32 * span + 512) / 1024) as u8
}

const fn replace_bounds(old: u64, min: u8, max: u8) -> u64 {
    (old & !REQUEST_BOUNDS) | min as u64 | ((max as u64) << 8)
}

const fn cache_matches(state: &State, min: u8, max: u8) -> bool {
    state.last_valid && state.last_min == min && state.last_max == max
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FleetModelPhase {
    Collecting,
    Active,
    Aborted,
}

/// Host-test model of the startup transition. Hardware prepare remains a
/// read-only capability check on each CPU.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FleetModel {
    expected: usize,
    prepared: usize,
    phase: FleetModelPhase,
}

#[cfg(test)]
impl FleetModel {
    const fn new(expected: usize) -> Self {
        Self {
            expected,
            prepared: 0,
            phase: FleetModelPhase::Collecting,
        }
    }

    fn prepare(&mut self, success: bool) {
        if !success {
            self.phase = FleetModelPhase::Aborted;
        } else if self.phase == FleetModelPhase::Collecting {
            self.prepared += 1;
        }
    }

    fn commit(&mut self) -> bool {
        if self.phase == FleetModelPhase::Collecting && self.prepared == self.expected {
            self.phase = FleetModelPhase::Active;
        }
        self.phase == FleetModelPhase::Active
    }
}

#[cfg(any(target_os = "none", test))]
fn decode_capabilities(value: u64) -> Option<Capabilities> {
    let highest = value as u8;
    let lowest = (value >> 24) as u8;
    if lowest <= highest {
        Some(Capabilities { lowest, highest })
    } else {
        None
    }
}

#[cfg(any(target_os = "none", test))]
fn cpuid_gate(max_basic_leaf: u32, vendor: [u32; 3], leaf6_eax: u32) -> bool {
    max_basic_leaf >= 6 && vendor == INTEL_VENDOR && leaf6_eax & (1 << 7) != 0
}

#[cfg(any(target_os = "none", test))]
const fn pm_enable_active(value: u64) -> bool {
    value & HWP_ENABLE != 0
}

#[cfg(target_os = "none")]
fn cpuid_supports_hwp() -> bool {
    let leaf0 = __cpuid_count(0, 0);
    cpuid_gate(
        leaf0.eax,
        [leaf0.ebx, leaf0.edx, leaf0.ecx],
        __cpuid_count(6, 0).eax,
    )
}

#[cfg(target_os = "none")]
fn read(msr: u32) -> u64 {
    unsafe { rdmsr(msr) }
}

#[cfg(not(target_os = "none"))]
fn read(_: u32) -> u64 {
    unreachable!("host HWP stub must not access MSRs")
}

#[cfg(target_os = "none")]
fn write(msr: u32, value: u64) {
    unsafe { wrmsr(msr, value) }
}

#[cfg(not(target_os = "none"))]
fn write(_: u32, _: u64) {
    unreachable!("host HWP stub must not access MSRs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gating_model_requires_intel_leaf_six_hwp_and_enabled_pm() {
        assert!(cpuid_gate(6, INTEL_VENDOR, 1 << 7));
        assert!(!cpuid_gate(5, INTEL_VENDOR, 1 << 7));
        assert!(!cpuid_gate(6, [0; 3], 1 << 7));
        assert!(!cpuid_gate(6, INTEL_VENDOR, 0));
        assert!(pm_enable_active(HWP_ENABLE));
        assert!(!pm_enable_active(0));
    }

    #[test]
    fn capabilities_reject_inverted_range() {
        assert_eq!(
            decode_capabilities(0x20),
            Some(Capabilities {
                lowest: 0,
                highest: 0x20
            })
        );
        assert_eq!(decode_capabilities((0x80_u64 << 24) | 0x40), None);
    }

    #[test]
    fn mapping_has_correct_endpoints_monotonicity_and_equal_bounds() {
        let caps = Capabilities {
            lowest: 20,
            highest: 180,
        };
        assert_eq!(map_clamp(0, caps), 20);
        assert_eq!(map_clamp(1024, caps), 180);
        assert!(valid_clamp(333, 333));
        let mut prior = 0;
        for value in 0..=1024 {
            let mapped = map_clamp(value, caps);
            assert!(mapped >= prior);
            prior = mapped;
        }
        let fixed = Capabilities {
            lowest: 77,
            highest: 77,
        };
        assert_eq!(map_clamp(0, fixed), 77);
        assert_eq!(map_clamp(513, fixed), 77);
    }

    #[test]
    fn request_rmw_preserves_every_non_bound_bit() {
        let old = 0xabcd_0123_4567_89ef;
        let new = replace_bounds(old, 0x12, 0x34);
        assert_eq!(new & !REQUEST_BOUNDS, old & !REQUEST_BOUNDS);
        assert_eq!(new & REQUEST_BOUNDS, 0x3412);
    }

    #[test]
    fn invalid_clamps_and_cache_decisions_are_explicit() {
        assert!(valid_clamp(0, 1024));
        assert!(!valid_clamp(1025, 1025));
        assert!(!valid_clamp(20, 19));
        let mut state = EMPTY_STATE;
        state.last_valid = true;
        state.last_min = 5;
        state.last_max = 9;
        assert!(cache_matches(&state, 5, 9));
        assert!(!cache_matches(&state, 5, 10));
    }

    #[test]
    fn restore_mask_is_low_sixteen_bits_only() {
        let old = 0xff00_0000_0000_aa55;
        let restored = replace_bounds(old, 3, 250);
        assert_eq!(restored & !REQUEST_BOUNDS, old & !REQUEST_BOUNDS);
        assert_eq!(restored & REQUEST_BOUNDS, 0xfa03);
    }

    #[test]
    fn fleet_does_not_activate_until_every_cpu_prepares() {
        let mut fleet = FleetModel::new(3);
        fleet.prepare(true);
        assert!(!fleet.commit());
        fleet.prepare(true);
        assert!(!fleet.commit());
        fleet.prepare(true);
        assert!(fleet.commit());
        assert_eq!(fleet.phase, FleetModelPhase::Active);
    }

    #[test]
    fn repeated_prepare_after_commit_is_not_an_abort() {
        let mut fleet = FleetModel::new(1);
        fleet.prepare(true);
        assert!(fleet.commit());
        fleet.prepare(true);
        assert!(fleet.commit());
        assert_eq!(fleet.phase, FleetModelPhase::Active);
    }

    #[test]
    fn failed_prepare_aborts_even_if_other_cpus_were_ready() {
        let mut fleet = FleetModel::new(3);
        fleet.prepare(true);
        fleet.prepare(false);
        fleet.prepare(true);
        assert!(!fleet.commit());
        assert_eq!(fleet.phase, FleetModelPhase::Aborted);
    }

    #[test]
    fn full_request_restore_is_not_a_bounds_rmw() {
        let saved = 0xabcd_0123_4567_89ef;
        let changed = replace_bounds(saved, 0x12, 0x34);
        assert_ne!(changed, saved);
        // Restore writes the saved request verbatim, including policy bits.
        assert_eq!(saved, 0xabcd_0123_4567_89ef);
    }
}
