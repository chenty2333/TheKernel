//! Opt-in Intel MONITOR/MWAIT idle, with a timer-bounded residency predictor.
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

/// An idle state's architectural hint and timing facts, in microseconds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdleState {
    pub name: &'static str,
    pub desc: &'static str,
    pub hint: u8,
    pub latency: u32,
    pub residency: u32,
    pub default_disabled: bool,
}
const HLT: IdleState = IdleState {
    name: "C1",
    desc: "HLT",
    hint: 0,
    latency: 1,
    residency: 1,
    default_disabled: false,
};
// Architectural facts from intel_idle gmt_cstates (Linux 7.2.3), not code.
pub const GRACEMONT: [IdleState; 5] = [
    IdleState {
        name: "C1",
        desc: "MWAIT 0x00",
        hint: 0,
        latency: 1,
        residency: 1,
        default_disabled: true,
    },
    IdleState {
        name: "C1E",
        desc: "MWAIT 0x01",
        hint: 1,
        latency: 2,
        residency: 4,
        default_disabled: false,
    },
    IdleState {
        name: "C6",
        desc: "MWAIT 0x20",
        hint: 0x20,
        latency: 195,
        residency: 585,
        default_disabled: false,
    },
    IdleState {
        name: "C8",
        desc: "MWAIT 0x40",
        hint: 0x40,
        latency: 260,
        residency: 1040,
        default_disabled: false,
    },
    IdleState {
        name: "C10",
        desc: "MWAIT 0x60",
        hint: 0x60,
        latency: 660,
        residency: 1980,
        default_disabled: false,
    },
];
// The development host is Panther Lake, not an N305. Use its own upstream
// table, never a spoofed Gracemont identity, for actual KVM MWAIT acceptance.
pub const PANTHER_LAKE: [IdleState; 4] = [
    IdleState {
        name: "C1",
        desc: "MWAIT 0x00",
        hint: 0,
        latency: 1,
        residency: 1,
        default_disabled: false,
    },
    IdleState {
        name: "C1E",
        desc: "MWAIT 0x01",
        hint: 1,
        latency: 10,
        residency: 10,
        default_disabled: false,
    },
    IdleState {
        name: "C6S",
        desc: "MWAIT 0x21",
        hint: 0x21,
        latency: 300,
        residency: 300,
        default_disabled: false,
    },
    IdleState {
        name: "C10",
        desc: "MWAIT 0x60",
        hint: 0x60,
        latency: 370,
        residency: 2500,
        default_disabled: false,
    },
];
const MAX_STATES: usize = 6;
struct CpuIdle {
    model: AtomicU8,
    eligible: AtomicU8,
    disabled: AtomicU8,
    usage: [AtomicU64; MAX_STATES],
    time: [AtomicU64; MAX_STATES],
    deadline: AtomicU64,
    prediction: AtomicU64,
}
impl CpuIdle {
    const fn new() -> Self {
        Self {
            model: AtomicU8::new(0),
            eligible: AtomicU8::new(1),
            disabled: AtomicU8::new(0),
            usage: [const { AtomicU64::new(0) }; MAX_STATES],
            time: [const { AtomicU64::new(0) }; MAX_STATES],
            deadline: AtomicU64::new(0),
            prediction: AtomicU64::new(0),
        }
    }
}
static CPUS: [CpuIdle; crate::config::plat::MAX_CPU_NUM] =
    [const { CpuIdle::new() }; crate::config::plat::MAX_CPU_NUM];
static OPTED_IN: AtomicBool = AtomicBool::new(false);
#[repr(align(64))]
struct MonitorLine(AtomicU64);
static MONITOR: [MonitorLine; crate::config::plat::MAX_CPU_NUM] =
    [const { MonitorLine(AtomicU64::new(0)) }; crate::config::plat::MAX_CPU_NUM];
fn table(model: u8) -> &'static [IdleState] {
    match model {
        0xbe => &GRACEMONT,
        0xcc => &PANTHER_LAKE,
        _ => &[],
    }
}
pub fn state(cpu: usize, index: usize) -> Option<IdleState> {
    let cpu = CPUS.get(cpu)?;
    if index == 0 {
        Some(HLT)
    } else {
        table(cpu.model.load(Ordering::Acquire))
            .get(index - 1)
            .copied()
    }
}
pub fn state_count(cpu: usize) -> usize {
    CPUS.get(cpu)
        .map_or(0, |cpu| 1 + table(cpu.model.load(Ordering::Acquire)).len())
}
pub fn enabled() -> bool {
    OPTED_IN.load(Ordering::Acquire)
}
pub fn supported(cpu: usize) -> bool {
    CPUS.get(cpu)
        .is_some_and(|cpu| cpu.eligible.load(Ordering::Acquire) & !1 != 0)
}
pub fn disabled(cpu: usize, index: usize) -> Option<bool> {
    state(cpu, index)?;
    let cpu = &CPUS[cpu];
    let bit = 1 << index;
    Some(
        cpu.eligible.load(Ordering::Acquire) & bit == 0
            || cpu.disabled.load(Ordering::Acquire) & bit != 0,
    )
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdleError {
    InvalidState,
}
pub fn set_disabled(cpu: usize, index: usize, value: bool) -> Result<(), IdleError> {
    state(cpu, index).ok_or(IdleError::InvalidState)?;
    let cpu = &CPUS[cpu];
    let bit = 1 << index;
    if index == 0 && value {
        return Err(IdleError::InvalidState);
    }
    if !value && cpu.eligible.load(Ordering::Acquire) & bit == 0 {
        return Err(IdleError::InvalidState);
    }
    if value {
        cpu.disabled.fetch_or(bit, Ordering::AcqRel);
    } else {
        cpu.disabled.fetch_and(!bit, Ordering::AcqRel);
    }
    Ok(())
}
pub fn counters(cpu: usize, index: usize) -> Option<(u64, u64)> {
    state(cpu, index)?;
    let cpu = &CPUS[cpu];
    Some((
        cpu.usage[index].load(Ordering::Relaxed),
        cpu.time[index].load(Ordering::Relaxed),
    ))
}
/// MWAIT's CPUID substate index is hint[7:4]+1, not the marketing C-number.
fn hint_available(substates: u32, hint: u8) -> bool {
    ((substates >> (((hint >> 4) as u32 + 1) * 4)) & 0xf) > u32::from(hint & 0xf)
}
fn eligible(model: u8, substates: u32, arat: bool) -> u8 {
    let mut mask = 1;
    for (index, state) in table(model).iter().enumerate() {
        if !state.default_disabled
            && hint_available(substates, state.hint)
            && (state.hint < 0x20 || arat)
        {
            mask |= 1 << (index + 1);
        }
    }
    mask
}
fn choose(states: &[IdleState], available: u8, disabled: u8, expected_us: u64) -> usize {
    let mut chosen = 0;
    for (index, state) in states.iter().enumerate() {
        let index = index + 1;
        if available & !disabled & (1 << index) != 0
            && expected_us >= u64::from(state.residency.max(state.latency))
        {
            chosen = index;
        }
    }
    chosen
}
/// Read-only CPU discovery, before userspace and before this CPU's idle loop.
pub fn init_current() {
    #[cfg(target_os = "none")]
    {
        use core::arch::x86_64::__cpuid_count;
        let (vendor, identity, mwait, thermal) = (
            __cpuid_count(0, 0),
            __cpuid_count(1, 0),
            __cpuid_count(5, 0),
            __cpuid_count(6, 0),
        );
        let cpu = axplat::percpu::this_cpu_id();
        let opted = crate::boot_command_line().and_then(|line| {
            line.split_ascii_whitespace()
                .filter_map(|p| p.strip_prefix("cpuidle.mwait="))
                .next_back()
        }) == Some("1");
        OPTED_IN.store(opted, Ordering::Release);
        let family = (identity.eax >> 8) & 15;
        let model = (((identity.eax >> 16) & 15) << 4 | ((identity.eax >> 4) & 15)) as u8;
        let intel = [vendor.ebx, vendor.edx, vendor.ecx] == [0x756e6547, 0x49656e69, 0x6c65746e];
        if vendor.eax < 6
            || !intel
            || family != 6
            || identity.ecx & (1 << 3) == 0
            || mwait.ecx & 3 != 3
            || table(model).is_empty()
        {
            return;
        }
        let mask = eligible(model, mwait.edx, thermal.eax & (1 << 2) != 0);
        let mut disable = 0;
        for (index, state) in table(model).iter().enumerate() {
            if state.default_disabled {
                disable |= 1 << (index + 1);
            }
        }
        CPUS[cpu].disabled.store(disable, Ordering::Relaxed);
        CPUS[cpu].eligible.store(mask, Ordering::Release);
        CPUS[cpu].model.store(model, Ordering::Release);
    }
}
/// Record the next LAPIC deadline without changing how that timer is armed.
pub fn note_timer(deadline_ns: u64) {
    #[cfg(target_os = "none")]
    CPUS[axplat::percpu::this_cpu_id()]
        .deadline
        .store(deadline_ns, Ordering::Relaxed);
    #[cfg(not(target_os = "none"))]
    let _ = deadline_ns;
}
/// Called with IRQs disabled after the scheduler's final ready-queue check.
/// Returns with IRQs enabled, like the default `sti; hlt` path.
pub fn wait() {
    #[cfg(target_os = "none")]
    {
        use axplat::time::{current_ticks, ticks_to_nanos};
        let cpu_id = axplat::percpu::this_cpu_id();
        let cpu = &CPUS[cpu_id];
        let before = ticks_to_nanos(current_ticks());
        let until_timer = cpu.deadline.load(Ordering::Relaxed).saturating_sub(before) / 1000;
        let history = cpu.prediction.load(Ordering::Relaxed);
        let expected = if history == 0 {
            until_timer
        } else {
            until_timer.min(history)
        };
        let chosen = if enabled() {
            choose(
                table(cpu.model.load(Ordering::Acquire)),
                cpu.eligible.load(Ordering::Acquire),
                cpu.disabled.load(Ordering::Acquire),
                expected,
            )
        } else {
            0
        };
        if chosen == 0 {
            axcpu::asm::enable_irqs_and_wait();
        } else {
            let hint = table(cpu.model.load(Ordering::Acquire))[chosen - 1].hint;
            // SAFETY: CPUID admits MONITOR/MWAIT extensions and masked-IRQ
            // wake; all hardware operands come from the validated table.
            // ECX[0] wakes on pending interrupts even with IF=0, so
            // a wake between the ready check and MWAIT is not lost.
            unsafe {
                core::arch::asm!("monitor", in("rax") &MONITOR[cpu_id].0 as *const AtomicU64, in("rcx") 0u64, in("rdx") 0u64, options(nostack));
            }
            unsafe {
                core::arch::asm!("mwait", in("eax") u32::from(hint), in("ecx") 1u32, options(nostack));
            }
        }
        // MWAIT returns with IF=0: account before dispatch/preemption.
        // HLT counters include the interrupt-return overhead.
        let elapsed = ticks_to_nanos(current_ticks()).saturating_sub(before) / 1000;
        cpu.usage[chosen].fetch_add(1, Ordering::Relaxed);
        cpu.time[chosen].fetch_add(elapsed, Ordering::Relaxed);
        cpu.prediction.store(
            if history == 0 {
                elapsed
            } else {
                history.saturating_mul(3).saturating_add(elapsed) / 4
            },
            Ordering::Relaxed,
        );
        axcpu::asm::enable_irqs();
    }
    #[cfg(not(target_os = "none"))]
    axcpu::asm::enable_irqs_and_wait();
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gracemont_facts_and_arat_gate() {
        assert_eq!(
            GRACEMONT.map(|s| (s.hint, s.latency, s.residency)),
            [
                (0, 1, 1),
                (1, 2, 4),
                (0x20, 195, 585),
                (0x40, 260, 1040),
                (0x60, 660, 1980)
            ]
        );
        assert_eq!(eligible(0xbe, 0x11111120, false), 5);
        assert_eq!(eligible(0xbe, 0x11111120, true), 61);
        assert_eq!(eligible(0xff, u32::MAX, true), 1);
        assert!(!hint_available(0x10, 1));
        assert!(hint_available(0x20, 1));
    }
    #[test]
    fn residency_and_disable_boundaries() {
        assert_eq!(choose(&GRACEMONT, 63, 2, 3), 0);
        assert_eq!(choose(&GRACEMONT, 63, 2, 4), 2);
        assert_eq!(choose(&GRACEMONT, 63, 2, 584), 2);
        assert_eq!(choose(&GRACEMONT, 63, 2, 585), 3);
        assert_eq!(choose(&GRACEMONT, 7, 2, u64::MAX), 2);
        assert_eq!(choose(&GRACEMONT, 63, 2, 1980), 5);
        assert_eq!(choose(&GRACEMONT, 63, 0xff, u64::MAX), 0);
        assert!(set_disabled(usize::MAX, 0, false).is_err());
        assert!(set_disabled(0, 0, true).is_err());
        assert_eq!(disabled(0, 0), Some(false));
        assert!(set_disabled(0, 0, false).is_ok());
        assert!(set_disabled(0, 1, false).is_err());
    }
}
