# CPU power management (x86_64)

## Scope and safety

All Rust is original. Behavioral authority is Linux 7.2.3 `intel_idle.c`,
`intel_pstate.c`, `coretemp.c`; register facts come from CPUID/MSRs.
**未在硬件上验证**. No NVMe writes or firmware HWP enablement are involved.

## Idle: automatic admission, explicit troubleshooting disable

Default is automatic MWAIT **when the current CPU and platform pass admission**.
Otherwise the scheduler reliably uses `sti; hlt`. This supersedes the former
opt-in default; it does not change HWP policy, NVMe policy or thermal registers.

`cpuidle.mwait` semantics:
- absent: automatic admission (the normal default);
- `0`: force HLT, without hiding the detected hardware/state table;
- `1`: same automatic admission as absent, **not** a hardware/deep-state override;
- empty, bare or any other value: fail closed to HLT;
- when repeated, the last exact key wins.

Every CPU must report Intel family6, an explicitly known model (BE Gracemont or
CC Panther Lake), CPUID basic leaf6, MONITOR, CPUID.5 extension and masked-IRQ
break support, and advertised substates for the chosen hint. Unknown hardware
is not guessed from model proximity. Gracemont uses upstream gmt facts:
C1 00/1/1 (unusable, cannot be enabled), C1E 01/2/4, C6 20/195/585,
C8 40/260/1040, C10 60/660/1980 (hint/latency/residency in microseconds).
Panther Lake uses its own upstream table, not spoofed Gracemont facts.
CPUID.5 substate numbering is hint high-nibble +1; the requested substate must
exist. C6 and deeper hints (>=20 hex) additionally require per-CPU CPUID.6 ARAT.
Automatic MWAIT does **not** bypass that gate or state disable controls.

The monotonic clock must survive idle: a selected TSC requires invariant TSC
on this CPU too, while the admitted 64-bit HPET is independent of CPU idle.
An uninitialized/unsafe clock excludes MWAIT. TSC-deadline or calibrated local
LAPIC one-shot must have actually been armed with a future deadline. Timer
programming is preemption/IRQ pinned; idle sees its deadline only after the
hardware arm succeeds. An absent/expired deadline always selects HLT, even if
idle-history prediction would otherwise suggest a long sleep.

The predictor uses the smaller of that deadline and an EWMA of recent idle
intervals; latency/residency must fit. This is a simplified governor, not Linux
menu/teo or full latency QoS. The scheduler's final ready check stays IRQ-disabled;
MONITOR/MWAIT uses masked-interrupt break, so an interrupt already pending or
arriving before entry is not lost. Accounting precedes IRQ delivery. Mandatory
HLT cannot be disabled. No firmware C-state policy/demotion MSRs are changed.

Sysfs state name/desc/latency/residency/usage/time/disable retain their units.
`mwait_enabled` means policy allows automatic admission, not that every CPU
can enter it. Per-CPU `mwait_supported` means safe platform admission; disabled
or too-short intervals can still select HLT. Counters count software entries/
wall microseconds, not hardware residency; HLT time includes IRQ-return overhead.

QEMU still needs explicit host permission `--accel kvm --cpu-pm`
(`-overcommit cpu-pm=on`) to expose/pass HLT and MWAIT. No kernel opt-in is
required. For troubleshooting use `--guest-kernel-cmdline cpuidle.mwait=0` in
the guest suite, or `--kernel-cmdline cpuidle.mwait=0` in a normal run.
Without advertised MWAIT, even the default automatically falls back to HLT.

## Current idle validation scope

Host tests cover parameter defaults/repetition/invalid input, known state facts,
feature/vendor/model admission, invariant clock, ARAT, disable, residency and
future-armed-deadline requirements. The guest power case checks each CPU's HLT
and MWAIT deltas separately: forced-off/unsupported paths must have zero MWAIT
entries. It also checks timer wake on every allowed CPU,100 timer-paced remote
futex handshakes and a one-second oversubscribed, pinned busy workload with
verified sums and a sleeping heartbeat. These are scheduling correctness
probes with bounded waits, **not** latency or energy benchmarks.

Measured with the formal framework, without repeating unrelated ABI/host suites:

| Path | Observation | Scheduling correctness |
| --- | --- | --- |
| Default KVM + `--cpu-pm`, no kernel parameter | Complete guest63/63 passed; all4 CPUs admitted MWAIT,326–372 entries/300 ms window | Timer wake on all CPUs,100 remote futex handshakes and8 pinned busy workers with heartbeat/sum checks passed |
| KVM + `--cpu-pm`, `cpuidle.mwait=0` | Focused shell CPU-power case passed; support remains1, all4 CPUs MWAIT entries/time0 and HLT grows | Same wake/load checks passed |
| Default KVM without `--cpu-pm`, no kernel parameter | Focused case passed; policy1, admission0, all4 CPUs MWAIT entries/time0 and HLT grows | Same wake/load checks passed |

Affected platform host tests passed143; harness host tests passed62 (3 skips),
strict C compilation/Linux-host scheduling probe and q35/n305 lint passed. Scope is
four virtual CPUs,100 handshakes and one-second load, not a long-duration soak.
No native authorization/run is included. Entry/time growth, virtual scheduling
stability, physical core/package residency and actual energy savings are
separate claims; the latter two remain **未在硬件上验证**. Host utilization under
concurrent builds/VMs is not evidence of savings.

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

## Read-only coretemp

Physical Intel family6 models BE/CC with DTS are admitted; hypervisors and
unknown models are excluded before any thermal MSR access. TjMax comes from
MSR_TEMPERATURE_TARGET[23:16], with Linux's target offset[15:8] used for max.
Input is (TjMax − digital readout) in millidegrees only when status valid[31]
is set. Crit alarm reports the out-of-spec log bit[5], without clearing it.
Only IA32_THERM_STATUS, optional package status and target are read.

Owner timer paths sample core/package status every100 ms; sysfs reads those
value snapshots, never another CPU's MSR. One coretemp hwmon per package
contains package and deduplicated core labels, input/max/crit/crit_alarm.
Attributes are read-only. Invalid samples return an error, not a made-up
temperature. Unsupported QEMU has an empty hwmon class, no pretend sensors.
The class composes with the existing dynamic device registry (graphics etc.).
Known scope differences: model admission is deliberately narrow, TjMax is
captured at boot, and no CPU-hotplug/sysfs sensor-removal lifecycle is added.
Native MSR admission and actual readings remain **未在硬件上验证**.

Temperature validation: platform139 tests passed (target/offset/validity/delta,
virtual/unknown admission); kernel read-only attribute test and q35 lint passed.
KVM guest63/63 (system-migo15ns) opened the empty hwmon class successfully,
without probing virtual thermal MSRs. Actual core/package readings remain
unverified on physical hardware.

## Real power tools

Nine pinned, signed Alpine3.24 runtime packages stage unmodified cpupower7.1.5
and sensors3.6.0 into the debug guest. Info commands run as uid/gid65534: their
sysfs inputs are public. This avoids cpupower's root-only attempt to load the
Linux msr module (TheKernel has no loadable modules); no fake successful
modprobe or MSR transport is installed. Sensors' status1/No sensors found is
the expected unsupported QEMU result, not a temperature measurement.

Before the automatic-default change, real-tool acceptance passed in debug KVM guest66/66
(system-13qkgs9m): cpupower idle-info reported intel_idle, HLT/MWAIT descriptions,
latency/residency and growing usage/duration; frequency-info honestly reported
no active driver, and sensors reported No sensors found. Running info commands
as an unprivileged user removed the initial root-only missing-msr-module warning.
The same run passed real GDB basic/watch/threads and strace-f. Final full host
passed654 Python tests (3 skips), kernel2599; affected tool fixtures refreshed
with63 host tests and strict C compilation. q35/n305 lint passed with existing
warnings; platform power excerpt scan found zero matches/zero Linux code.

## Earlier full ABI boundary (unchanged by idle policy)

Complete50-program ABI run `abi-wx81cz3w`: all50 TheKernel programs passed.
The Linux oracle completed50 with one unchanged socket-provider failure,
RCVTIMEO_JIFFY_ROUND_TRIP (its HZ100 timeout-rounding expectation). Thus the
complete paired gate is **not a pass**. It predates this continuation and is
outside the A task; no socket/B work was changed. The hardware ptrace subset
passed on both kernels separately. Power interfaces do not establish native
N305 behavior, physical residency or a controlled host-utilization decrease.

Known HWP difference: remote policy writes publish/kick asynchronously; sysfs
reads show requested policy, with hardware applying it on its owner refresh.
Native reproducibility tests must verify application before timing work and
configure every CPU used by the workload, not only cpu0.
