//! Read-only Intel DTS/coretemp, fail closed on virtual/unknown CPUs.
use kspin::SpinNoIrq;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Target {
    pub critical_mc: i32,
    pub maximum_mc: Option<i32>,
}
pub fn decode_target(raw: u64) -> Option<Target> {
    let critical = ((raw >> 16) & 0xff) as i32;
    if !(70..=125).contains(&critical) {
        return None;
    }
    let offset = ((raw >> 8) & 0xff) as i32;
    Some(Target {
        critical_mc: critical * 1000,
        maximum_mc: (offset <= critical).then_some((critical - offset) * 1000),
    })
}
pub fn temperature(raw: u64, target: Target) -> Option<i32> {
    (raw & (1 << 31) != 0).then_some(target.critical_mc - (((raw >> 16) & 0x7f) as i32) * 1000)
}
#[derive(Clone, Copy, Debug)]
pub struct Sensor {
    pub cpu: usize,
    pub package: u32,
    pub core: u32,
    pub target: Target,
    pub package_available: bool,
    pub core_status: u64,
    pub package_status: u64,
}
static CPUS: [SpinNoIrq<Option<Sensor>>; crate::config::plat::MAX_CPU_NUM] =
    [const { SpinNoIrq::new(None) }; crate::config::plat::MAX_CPU_NUM];
static TIMES: [core::sync::atomic::AtomicU64; crate::config::plat::MAX_CPU_NUM] =
    [const { core::sync::atomic::AtomicU64::new(0) }; crate::config::plat::MAX_CPU_NUM];
fn admitted(intel: bool, family: u32, model: u32, hypervisor: bool, thermal: u32) -> bool {
    intel && family == 6 && matches!(model, 0xbe | 0xcc) && !hypervisor && thermal & 1 != 0
}
pub fn init_current() {
    #[cfg(target_os = "none")]
    {
        use core::arch::x86_64::__cpuid_count;
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let vendor = __cpuid_count(0, 0);
        if vendor.eax < 6 {
            return;
        }
        let id = __cpuid_count(1, 0);
        let therm = __cpuid_count(6, 0).eax;
        let model = ((id.eax >> 16) & 15) << 4 | ((id.eax >> 4) & 15);
        if !admitted(
            [vendor.ebx, vendor.edx, vendor.ecx] == [0x756e6547, 0x49656e69, 0x6c65746e],
            (id.eax >> 8) & 15,
            model,
            id.ecx & (1 << 31) != 0,
            therm,
        ) {
            return;
        }
        let cpu = axplat::percpu::this_cpu_id();
        let Some(topology) = crate::cpu::topology_for_logical(cpu) else {
            return;
        };
        // SAFETY: allowlisted modern physical Intel DTS CPU; no virtual MSRs.
        let Some(target) = decode_target(unsafe { x86::msr::rdmsr(0x1a2) }) else {
            return;
        };
        *CPUS[cpu].lock() = Some(Sensor {
            cpu,
            package: topology.package_id,
            core: topology.core_id,
            target,
            package_available: therm & (1 << 6) != 0,
            core_status: 0,
            package_status: 0,
        });
        sample_current(100_000_000);
    }
}
pub fn snapshot(cpu: usize) -> Option<Sensor> {
    *CPUS.get(cpu)?.lock()
}
/// Sample locally at most ten times a second. Never write thermal MSRs.
pub fn sample_current(now_ns: u64) {
    #[cfg(target_os = "none")]
    {
        use core::sync::atomic::Ordering;
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let cpu = axplat::percpu::this_cpu_id();
        let mut state = CPUS[cpu].lock();
        let Some(sensor) = state.as_mut() else {
            return;
        };
        if now_ns.saturating_sub(TIMES[cpu].load(Ordering::Relaxed)) < 100_000_000 {
            return;
        }
        // SAFETY: sensor publication required the physical CPU/MSR gates.
        unsafe {
            sensor.core_status = x86::msr::rdmsr(0x19c);
            if sensor.package_available {
                sensor.package_status = x86::msr::rdmsr(0x1b1);
            }
        }
        TIMES[cpu].store(now_ns, Ordering::Relaxed);
    }
    #[cfg(not(target_os = "none"))]
    let _ = now_ns;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn temperature_target_delta_validity_and_alarm_bits() {
        let target = decode_target((105 << 16) | (10 << 8)).unwrap();
        assert_eq!(target.critical_mc, 105000);
        assert_eq!(target.maximum_mc, Some(95000));
        assert_eq!(temperature((1 << 31) | (15 << 16), target), Some(90000));
        assert_eq!(temperature(15 << 16, target), None);
        assert_eq!(temperature((1 << 31) | (127 << 16), target), Some(-22000));
        assert!(decode_target(0).is_none());
        assert!(decode_target(126 << 16).is_none());
        assert_eq!(
            decode_target((100 << 16) | (255 << 8)).unwrap().maximum_mc,
            None
        );
    }
    #[test]
    fn no_msr_probe_on_qemu_or_unknown_model() {
        assert!(admitted(true, 6, 0xbe, false, 1));
        assert!(admitted(true, 6, 0xcc, false, 1));
        assert!(!admitted(true, 6, 0xbe, true, u32::MAX));
        assert!(!admitted(true, 6, 0xff, false, 1));
        assert!(!admitted(false, 6, 0xbe, false, 1));
        assert!(!admitted(true, 6, 0xbe, false, 0));
        assert!(snapshot(0).is_none());
    }
}
