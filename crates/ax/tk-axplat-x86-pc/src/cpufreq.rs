//! Explicit HWP policy; CPU frequency units are kHz, not performance ratios.
use kspin::SpinNoIrq;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Unsupported,
    InvalidInput,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrequencyCaps {
    pub lowest: u8,
    pub highest: u8,
    pub guaranteed: u8,
    pub nominal_khz: u32,
}
impl FrequencyCaps {
    pub fn frequency(self, perf: u8) -> u32 {
        ((u64::from(self.nominal_khz) * u64::from(perf)) / u64::from(self.guaranteed)) as u32
    }
    fn normalized(self, perf: u8) -> u16 {
        if self.highest == self.lowest {
            0
        } else {
            (((perf - self.lowest) as u32 * 1024 + (self.highest - self.lowest) as u32 / 2)
                / (self.highest - self.lowest) as u32) as u16
        }
    }
    fn performance(self, clamp: u16) -> u8 {
        (self.lowest as u32 + (clamp as u32 * (self.highest - self.lowest) as u32 + 512) / 1024)
            as u8
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    pub caps: FrequencyCaps,
    pub min: u8,
    pub max: u8,
    pub performance: bool,
    pub epp: Option<u8>,
    pub saved_epp: Option<u8>,
    pub explicit: bool,
    pub current_khz: Option<u32>,
}
impl Policy {
    pub fn update(&mut self, field: &str, value: &str) -> Result<(), Error> {
        let value = value.trim();
        let mut next = *self;
        match field {
            "scaling_governor" => match value {
                "performance" => {
                    next.performance = true;
                    next.epp = next.epp.map(|_| 0);
                }
                "powersave" => {
                    next.performance = false;
                    next.epp = next.epp.map(|_| 128);
                }
                _ => return Err(Error::InvalidInput),
            },
            "scaling_min_freq" | "scaling_max_freq" => {
                let khz = value.parse::<u32>().map_err(|_| Error::InvalidInput)?;
                if khz < self.caps.frequency(self.caps.lowest)
                    || khz > self.caps.frequency(self.caps.highest)
                {
                    return Err(Error::InvalidInput);
                }
                if field == "scaling_min_freq" {
                    next.min = (self.caps.lowest..=self.caps.highest)
                        .find(|&p| self.caps.frequency(p) >= khz)
                        .ok_or(Error::InvalidInput)?;
                } else {
                    next.max = (self.caps.lowest..=self.caps.highest)
                        .rev()
                        .find(|&p| self.caps.frequency(p) <= khz)
                        .ok_or(Error::InvalidInput)?;
                }
                if next.min > next.max {
                    return Err(Error::InvalidInput);
                }
            }
            "energy_performance_preference" => {
                next.epp = Some(match value {
                    "default" => self.saved_epp.ok_or(Error::Unsupported)?,
                    "performance" => 0,
                    "balance_performance" => 128,
                    "balance_power" => 192,
                    "power" => 255,
                    _ => value.parse().map_err(|_| Error::InvalidInput)?,
                });
                if self.epp.is_none() {
                    return Err(Error::Unsupported);
                }
                if self.performance && next.epp != Some(0) {
                    return Err(Error::InvalidInput);
                }
            }
            _ => return Err(Error::InvalidInput),
        }
        next.explicit = true;
        *self = next;
        Ok(())
    }
    fn merged(self, min: u16, max: u16) -> (u16, u16) {
        // User policy is a hard range. Scheduler demand is clipped into it;
        // a ceiling wins over a conflicting demand within that hard range.
        let high = self.caps.performance(max).clamp(self.min, self.max);
        let low = if self.performance {
            high
        } else {
            self.caps.performance(min).clamp(self.min, high)
        };
        (self.caps.normalized(low), self.caps.normalized(high))
    }
}
struct State {
    policy: Option<Policy>,
    aperf: u64,
    mperf: u64,
    sampled_at: u64,
}
static CPUS: [SpinNoIrq<State>; crate::config::plat::MAX_CPU_NUM] = [const {
    SpinNoIrq::new(State {
        policy: None,
        aperf: 0,
        mperf: 0,
        sampled_at: 0,
    })
};
    crate::config::plat::MAX_CPU_NUM];
/// APERF/MPERF deltas represent effective busy frequency; idle is excluded.
pub fn effective_frequency(nominal_khz: u32, aperf: u64, mperf: u64) -> Option<u32> {
    if mperf == 0 {
        return None;
    }
    Some(
        ((u128::from(nominal_khz) * u128::from(aperf)) / u128::from(mperf))
            .min(u128::from(u32::MAX)) as u32,
    )
}
pub fn init_current() {
    #[cfg(all(target_os = "none", feature = "hwp"))]
    {
        use core::arch::x86_64::__cpuid_count;
        let leaf = __cpuid_count(0, 0);
        if leaf.eax < 0x16 || __cpuid_count(6, 0).ecx & 1 == 0 {
            return;
        }
        let Ok((caps, guaranteed, request)) = crate::hwp::prepared_request_current() else {
            return;
        };
        if request & (1 << 42) != 0 {
            return;
        } // package-controlled requests are not per-CPU policy
        let nominal = __cpuid_count(0x16, 0).eax & 0xffff;
        if nominal == 0 || caps.lowest == 0 || guaranteed < caps.lowest || guaranteed > caps.highest
        {
            return;
        }
        let caps = FrequencyCaps {
            lowest: caps.lowest,
            highest: caps.highest,
            guaranteed,
            nominal_khz: nominal * 1000,
        };
        let min = (request as u8).clamp(caps.lowest, caps.highest);
        let requested_max = (request >> 8) as u8;
        let max = if requested_max == 0 {
            caps.highest
        } else {
            requested_max.clamp(min, caps.highest)
        };
        let epp = (__cpuid_count(6, 0).eax & (1 << 10) != 0).then_some((request >> 24) as u8);
        let cpu = axplat::percpu::this_cpu_id();
        let mut state = CPUS[cpu].lock();
        state.policy = Some(Policy {
            caps,
            min,
            max,
            performance: false,
            epp,
            saved_epp: epp,
            explicit: false,
            current_khz: None,
        });
        // SAFETY: CPUID admits Intel HWP and architectural APERF/MPERF.
        unsafe {
            state.aperf = x86::msr::rdmsr(0xe8);
            state.mperf = x86::msr::rdmsr(0xe7);
        }
    }
}
pub fn snapshot(cpu: usize) -> Option<Policy> {
    #[cfg(all(target_os = "none", feature = "hwp"))]
    {
        if !crate::hwp::is_active() {
            return None;
        }
        CPUS.get(cpu)?.lock().policy
    }
    #[cfg(not(all(target_os = "none", feature = "hwp")))]
    {
        let _ = cpu;
        None
    }
}
pub fn update(cpu: usize, field: &str, value: &str) -> Result<(), Error> {
    snapshot(cpu).ok_or(Error::Unsupported)?;
    CPUS.get(cpu)
        .ok_or(Error::Unsupported)?
        .lock()
        .policy
        .as_mut()
        .ok_or(Error::Unsupported)?
        .update(field, value)
}
pub fn explicit_current() -> bool {
    #[cfg(target_os = "none")]
    {
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        CPUS[axplat::percpu::this_cpu_id()]
            .lock()
            .policy
            .is_some_and(|p| p.explicit)
    }
    #[cfg(not(target_os = "none"))]
    false
}
#[cfg(feature = "hwp")]
pub fn merge_current_clamp(min: u16, max: u16) -> Result<(u16, u16), crate::hwp::Error> {
    #[cfg(target_os = "none")]
    {
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let policy = CPUS[axplat::percpu::this_cpu_id()]
            .lock()
            .policy
            .ok_or(crate::hwp::Error::Unsupported)?;
        let (low, high) = policy.merged(min, max);
        crate::hwp::set_policy_bits_current(
            if policy.performance {
                policy.caps.performance(high)
            } else {
                0
            },
            policy.epp,
        )?;
        Ok((low, high))
    }
    #[cfg(not(target_os = "none"))]
    {
        let _ = (min, max);
        Err(crate::hwp::Error::Unsupported)
    }
}
/// Sample on the owner's timer path, at most once per 100 ms. No remote MSRs.
pub fn sample_current(now_ns: u64) {
    #[cfg(all(target_os = "none", feature = "hwp"))]
    {
        if !crate::hwp::is_active() {
            return;
        }
        let _guard = kernel_guard::NoPreemptIrqSave::new();
        let cpu = axplat::percpu::this_cpu_id();
        let mut state = CPUS[cpu].lock();
        let Some(policy) = state.policy else {
            return;
        };
        if now_ns.saturating_sub(state.sampled_at) < 100_000_000 {
            return;
        }
        // SAFETY: initialized policy implies checked APERF/MPERF admission.
        let (aperf, mperf) = unsafe { (x86::msr::rdmsr(0xe8), x86::msr::rdmsr(0xe7)) };
        let khz = effective_frequency(
            policy.caps.nominal_khz,
            aperf.wrapping_sub(state.aperf),
            mperf.wrapping_sub(state.mperf),
        );
        if let Some(khz) = khz {
            state.policy.as_mut().unwrap().current_khz = Some(khz);
        }
        state.aperf = aperf;
        state.mperf = mperf;
        state.sampled_at = now_ns;
    }
    #[cfg(not(all(target_os = "none", feature = "hwp")))]
    let _ = now_ns;
}
#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            caps: FrequencyCaps {
                lowest: 10,
                highest: 50,
                guaranteed: 30,
                nominal_khz: 3_000_000,
            },
            min: 10,
            max: 50,
            performance: false,
            epp: Some(128),
            saved_epp: Some(192),
            explicit: false,
            current_khz: None,
        }
    }
    #[test]
    fn policy_ranges_governor_and_atomic_rejection() {
        let mut p = policy();
        assert!(!p.explicit);
        p.update("scaling_min_freq", "2000000\n").unwrap();
        assert_eq!(p.min, 20);
        p.update("scaling_max_freq", "4000000").unwrap();
        assert_eq!(p.max, 40);
        assert!(p.update("scaling_min_freq", "4000001").is_err());
        assert_eq!(p.min, 20);
        for value in ["0", "5000001", "-1", "4294967296", "1 2"] {
            assert!(p.update("scaling_max_freq", value).is_err());
        }
        p.update("scaling_governor", "performance").unwrap();
        assert_eq!(p.epp, Some(0));
        assert!(p.update("energy_performance_preference", "power").is_err());
        p.update("scaling_governor", "powersave").unwrap();
        p.update("energy_performance_preference", "default")
            .unwrap();
        assert_eq!(p.epp, Some(192));
        assert!(p.update("energy_performance_preference", "256").is_err());
        assert!(p.update("scaling_governor", "ondemand").is_err());
    }
    #[test]
    fn policy_bounds_override_scheduler_demand() {
        let mut p = policy();
        p.min = 20;
        p.max = 40;
        let (low, high) = p.merged(0, 0);
        assert_eq!(
            (p.caps.performance(low), p.caps.performance(high)),
            (20, 20)
        );
        let (low, high) = p.merged(1024, 1024);
        assert_eq!(
            (p.caps.performance(low), p.caps.performance(high)),
            (40, 40)
        );
        p.performance = true;
        assert_eq!(p.merged(0, 1024), (768, 768));
    }
    #[test]
    fn frequency_uses_wide_ratios_and_missing_delta_is_not_zero() {
        assert_eq!(effective_frequency(2_000_000, 15, 10), Some(3_000_000));
        assert_eq!(effective_frequency(1, 0, 10), Some(0));
        assert_eq!(
            effective_frequency(3_000_000, u64::MAX, u64::MAX),
            Some(3_000_000)
        );
        assert_eq!(effective_frequency(u32::MAX, u64::MAX, 1), Some(u32::MAX));
        assert_eq!(effective_frequency(1, 1, 0), None);
        assert!(snapshot(0).is_none());
        assert_eq!(
            update(0, "scaling_governor", "performance"),
            Err(Error::Unsupported)
        );
    }
}
