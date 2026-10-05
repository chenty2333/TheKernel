# CPU power management (x86_64)

## Scope and safety

All Rust is original. Behavioral authority is Linux 7.2.3 `intel_idle.c`,
`intel_pstate.c`, `coretemp.c`; register facts come from CPUID/MSRs.
**未在硬件上验证**. No NVMe writes or firmware HWP enablement are involved.

## Idle

Default stays `sti; hlt`. Only `cpuidle.mwait=1` selects MWAIT, and only on
known Intel family-6 models with CPUID MONITOR, MWAIT extensions and masked-IRQ
break support. Gracemont model BE uses gmt timing/hint facts: C1 00/1/1
(unusable, cannot be re-enabled), C1E 01/2/4, C6 20/195/585, C8 40/260/1040,
C10 60/660/1980 (hint/exit latency/target residency in microseconds).
Panther Lake model CC uses its own upstream table for development-host testing.
Unknown models/capabilities fall back to HLT. CPUID.5 substate admission uses
hint high-nibble + 1; without CPUID.6 ARAT no hint at or below C6 is admitted.

The predictor is the smaller of the next programmed LAPIC deadline and an
EWMA of recent idle intervals. It selects the deepest enabled state whose
residency and exit latency fit. This is not Linux menu/teo and has no full
latency-QoS governor. The scheduler keeps IRQs disabled through its final ready
check and MONITOR/MWAIT; ECX interrupt-break closes the wake race. Accounting
finishes before IRQ delivery. A mandatory HLT fallback cannot be disabled.

Sysfs exposes per-CPU state name/desc/latency/residency/usage/time/disable;
time is accumulated wall microseconds, not a hardware residency MSR.
HLT accounting includes interrupt-return overhead. Unsupported and ARAT-blocked
states cannot be re-enabled. A global diagnostic `mwait_enabled` and per-CPU
`mwait_supported` distinguish opt-in from availability.

QEMU opt-in: `--accel kvm --cpu-pm`, which adds `-overcommit cpu-pm=on`.
Guest suite boot opt-in: `--guest-kernel-cmdline cpuidle.mwait=1`.
This changes both HLT and MWAIT virtualization. Host utilization comparisons
under concurrent builds/other VMs are not performance evidence.

## Validation

Unit tests cover Gracemont facts, CPUID hint numbering, ARAT gate, residency
boundaries, disable and unsupported inputs. KVM MWAIT guest passed 63/63
(system-k_vuyw7g): all four CPUs reported MWAIT supported/enabled and usage
increased 234–383 entries during a 300 ms idle observation, with 198–299 ms
accumulated time. Default KVM guest passed 63/63 (system-3o8tk3d1), with MWAIT
not exposed/enabled and HLT counters increasing. Full host passed 653 Python
(3 skips) and kernel 2597; q35/n305 lint passed. These are software-entered idle
counters, not proof of physical package residency or power reduction.
A controlled host-utilization reduction has not been established: concurrent
Codex builds/VMs invalidate that performance comparison per COMMON rule 11.
Frequency, temperature and real-tool sections follow in independent changes.
