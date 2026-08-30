//! Current-CPU Intel Hardware P-state (HWP) clamp control.
//!
//! This module deliberately does not enable HWP: firmware must have set
//! `IA32_PM_ENABLE.HWP_ENABLE` before initialization.  It exclusively owns
//! bits 0..=15 (minimum/maximum performance) of `IA32_HWP_REQUEST`; callers
//! that change those bits outside this module must reinitialize before relying
//! on its write-elision cache.

#[cfg(target_os = "none")]
use core::arch::x86_64::__cpuid_count;

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
    initialized: bool,
    active: bool,
    caps: Capabilities,
    saved_min: u8,
    saved_max: u8,
    last_min: u8,
    last_max: u8,
    last_valid: bool,
}

const EMPTY_STATE: State = State {
    initialized: false,
    active: false,
    caps: Capabilities {
        lowest: 0,
        highest: 0,
    },
    saved_min: 0,
    saved_max: 0,
    last_min: 0,
    last_max: 0,
    last_valid: false,
};

// State is accessed only through `local`, which pins execution and disables
// interrupts before indexing the current logical CPU. No API accepts a CPU ID.
static STATES: [SpinNoIrq<State>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(EMPTY_STATE) }; crate::config::plat::MAX_CPU_NUM];

/// Detect and initialize HWP for the current CPU, then apply the unconstrained
/// `0..=1024` clamp. Repeated calls are harmless.
pub fn init_current() -> Result<(), Error> {
    local(|state| {
        if state.initialized {
            return state.active.then_some(()).ok_or(Error::Unsupported);
        }
        state.initialized = true;

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
            let caps = decode_capabilities(read(IA32_HWP_CAPABILITIES)).ok_or(Error::Unsupported)?;
            let saved = read(IA32_HWP_REQUEST);
            state.active = true;
            state.caps = caps;
            state.saved_min = saved as u8;
            state.saved_max = (saved >> 8) as u8;
            state.last_valid = false;
            apply(state, 0, 1024)
        }
    })
}

/// Return the HWP performance range detected for the current CPU.
pub fn capabilities() -> Result<Capabilities, Error> {
    local(|state| state.active.then_some(state.caps).ok_or(Error::Unsupported))
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
        if !state.active {
            return Err(Error::Unsupported);
        }
        apply(state, min, max)
    })
}

/// Restore the saved minimum and maximum HWP request bytes on the current CPU.
/// The operation is idempotent and preserves every other request bit.
pub fn restore_current_request() -> Result<(), Error> {
    local(|state| {
        if !state.active {
            return Err(Error::Unsupported);
        }
        if cache_matches(state, state.saved_min, state.saved_max) {
            return Ok(());
        }
        let old = read(IA32_HWP_REQUEST);
        write(
            IA32_HWP_REQUEST,
            replace_bounds(old, state.saved_min, state.saved_max),
        );
        state.last_min = state.saved_min;
        state.last_max = state.saved_max;
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
        assert_eq!(decode_capabilities(0x20), Some(Capabilities { lowest: 0, highest: 0x20 }));
        assert_eq!(decode_capabilities((0x80_u64 << 24) | 0x40), None);
    }

    #[test]
    fn mapping_has_correct_endpoints_monotonicity_and_equal_bounds() {
        let caps = Capabilities { lowest: 20, highest: 180 };
        assert_eq!(map_clamp(0, caps), 20);
        assert_eq!(map_clamp(1024, caps), 180);
        assert!(valid_clamp(333, 333));
        let mut prior = 0;
        for value in 0..=1024 {
            let mapped = map_clamp(value, caps);
            assert!(mapped >= prior);
            prior = mapped;
        }
        let fixed = Capabilities { lowest: 77, highest: 77 };
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
}
