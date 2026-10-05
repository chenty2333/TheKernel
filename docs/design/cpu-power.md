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

## Frequency and firmware ownership

Only firmware-enabled, fleet-admitted Intel HWP is exposed as intel_pstate.
No PM_ENABLE write is added. Default scheduler callbacks now leave the saved
firmware request untouched, including abort of a read-only prepare. A CPU's
policy becomes active only after an owner-writable sysfs policy write. Then
its owner applies it locally via the existing scheduler HWP refresh/IPI lane.
Bounds, desired performance and optional EPP are RMW; other request bits survive.
No remote CPU's MSR is accessed by a sysfs reader/writer.

User min/max are hard bounds; scheduler uclamp demand is clipped into that
range. Within the range a scheduler ceiling wins over a conflicting minimum.
Performance governor requests the resulting ceiling with EPP=0; powersave
leaves desired performance autonomous with a selected EPP. Numeric EPP 0–255
and default/performance/balance_performance/balance_power/power are accepted;
performance governor rejects a nonzero EPP. Invalid/range-inverted writes are
atomic failures. A policy write does not enable HWP in firmware.

Frequency conversion anchors HWP guaranteed performance to CPUID.16 nominal
kHz; min/max writes round to actual performance levels. This linear reference
is an approximation on hybrid CPUs, not complete Linux per-model/hybrid
calibration. Missing nominal/guaranteed/APERF-MPERF capabilities or a firmware
package-controlled request (bit42) suppress cpufreq rather than invent values.
APERF/MPERF is sampled on each owner's timer path at most once per100 ms;
scaling_cur_freq is its last busy-frequency ratio, not instantaneous wall rate.
A missing initial nonzero MPERF delta reports an error, not a fabricated rate.
Without support cpu*/cpufreq is absent and cpufreq_supported is0, allowing
cpupower to report no active driver. Hardware HWP behavior remains unverified.

Frequency validation: platform137 host tests passed, including range/governor/EPP
atomic rejection, policy/uclamp priority, wide APERF/MPERF ratios and unsupported
hardware. Kernel preference-name test and q35 lint passed. KVM guest63/63
(system-up8kt0mm) confirmed cpufreq_supported=0 and no phantom driver directory;
no HWP/APERF hardware effect is claimed. Owner sampling is preemption/IRQ pinned.
