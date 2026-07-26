# Nightly system adapters

These adapters exercise a real guest path and return one of three outcomes:

- `0`: the named semantic checks ran and passed;
- `1`: infrastructure launched the check, but the check failed;
- `78`: required infrastructure or an explicit capability was unavailable.

Exit `78` is not a pass. `nightly-gate.sh` records it as `unsupported` and
returns `78` after all enabled categories have been considered.

The repository-owned adapters cover:

- `pressure.sh`: bounded concurrent task and root-filesystem pressure while a
  multi-threaded scheduler workload runs, on each selected architecture;
- `oom-failpoint.sh`: deterministic anonymous-mapping admission failure under
  strict overcommit policy, policy restoration, and a successful recovery
  mapping on each selected architecture;
- `fs-powercut.sh`: a writable ext4 image, a durable write, abrupt QEMU
  `SIGKILL` after an exact guest marker, a second recovery boot, clean unmount,
  and host `e2fsck` verification;
- `nonloopback-network.sh`: a nonce-authenticated TCP exchange from the guest
  VirtIO NIC through QEMU user networking to a one-shot host peer;
- `smp-tlb-shootdown.sh`: an RV/LoongArch matrix at 4 and 8 requested CPUs.
  Before mutating page tables, it pins two syscall-free, non-yielding spin tasks
  to every online CPU and requires both to advance in each of three consecutive
  one-second windows. For every non-control CPU it then warms translations
  remotely, keeps that worker in a syscall-free heartbeat spin across the
  page-table mutation, verifies its actual CPU before and after the critical
  accesses, and checks 1-page and 64-page `mprotect`, `munmap` plus fixed
  replacement, fixed `mremap`, and fork COW transitions. Any stalled liveness
  window, stale access, incomplete CPU/case matrix, topology mismatch, or
  operational failure rejects the run;
- `mm-performance.sh`: an RV/LoongArch matrix at 4 and 8 requested CPUs. It
  records VMA-scale mapping latency, private-anonymous `mremap` resize latency,
  fixed replacement while retaining the declared sparse VMA fixture,
  two-CPU fixed `mremap` contention over disjoint slots in one address space,
  regular-file `MAP_SHARED` `old_size == 0` duplication, shared-anonymous
  grow/shrink, an `mprotect` plus touch TLB-sensitive proxy, and regular-file
  direct-I/O latency, throughput, same-address-space contention, and
  cross-address-space contention proxies. VMA-scale, both fixed-remap cases,
  and all three direct-I/O proxy
  records distinguish the requested fixture size from the live VMA count
  verified through `/proc/self/maps`. The fixture uses
  spaced `MAP_FIXED_NOREPLACE` slots; unsupported fixed placement or proc maps
  is explicit missing evidence. The disjoint remap workers use four
  non-overlapping slots, retain per-page sentinels, bind to distinct CPUs, and
  publish execution windows that must overlap. The run record binds the system
  page size, and the affinity record carries the ordered guest CPU IDs; the
  parser requires every worker CPU to occur in that witness and checks that the
  two-page remap slots are page-aligned. Direct-I/O workers are bound to
  distinct guest CPUs and do their warmup after binding; the proxy metrics retain the
  requested sparse VMA fixture for their complete measured phase. The
  cross-address-space case verifies the fixture in the parent, then forks one
  process per worker so the exact fixture is inherited into independent
  address spaces. Each child creates its direct-I/O file and aligned buffer
  after the fork, warms up before a pipe barrier, and publishes samples through
  bounded shared anonymous storage. Raw worker records include distinct child
  PIDs, a private COW-isolation witness, fixed CPU placement, and pre/post
  fixture counts. Every metric
  contains count/p50/p99/p999; unavailable paths remain explicit `missing`
  records with a reason and errno, and make the mandatory matrix fail instead
  of being reported as a completed baseline. A cleanup failure also invalidates
  an otherwise successful metric, so leaked fixtures cannot silently influence
  a later case.

These gates deliberately do not overclaim. The OOM adapter does not substitute
for a future kernel-allocator failpoint framework or OOM-victim policy test.
The network adapter proves a real non-loopback NIC path but does not substitute
for TAP, packet loss, multi-peer, or physical-NIC testing. The power-cut test
models sudden VM process loss after explicit durable writes; storage devices
with volatile caches still require hardware-appropriate flush/fence testing.
The MM adapter verifies that the guest actually brought the requested CPU count
online and rejects a topology mismatch. Its protect-and-touch metric is a
user-visible TLB-sensitive proxy, not a hardware TLB-shootdown event counter.
Likewise, concurrent direct I/O is an end-to-end proxy. A successful write does
not by itself prove that the kernel selected its short-pin path; the opt-in lock
diagnostic run separately proves whether all pin stages were exercised. The
cross-address-space case can expose contention in globally shared state, but no
proxy metric isolates time spent in one particular shard lock. The disjoint
`mremap` case proves that two workload windows overlap and measures the current
serialized path; it does not claim a lockless MM design or prove that every pair
of syscalls overlapped.
The standalone
parser can still normalize an explicit `missing` record for diagnostic use,
but the nightly adapter requires all ten metrics to be present.

The SMP TLB adapter is a semantic gate, not a hardware event counter. Its
qualification must include a mutation run from a disposable build in which
the target still receives the IPI, consumes the maintenance reason, and
publishes its completion epoch, but deliberately omits only the local TLB
invalidation: at least one warmed remote access must produce a `status=stale`
result and the adapter must fail. Do not add that fault injection as a
production default or accept a clean pass from the known-bad build. Timer
preemption can still evict a translation naturally, so
the matrix covers every remote CPU and both a single hot page and a range; the
mutation run remains the proof that the workload can turn the intended defect
red. Validate that expected-failure log with
`validate-smp-tlb-log.sh LOG EXPECTED_CPUS stale`; this mode requires a complete
case matrix, at least one actual stale case, and an exact aggregate stale marker.
It rejects timeouts and operational failures.

`THEKERNEL_NIGHTLY_ARCHES` accepts `rv`, `la`, or `both` (the default). Missing
QEMU binaries, cross compilers, rootfs build tools, or filesystem tools cause
exit `78`. A runner may provide a category-specific `*_COMMAND` override for
hardware-only testing; its exit `78` retains the same unsupported meaning.

The MM matrix defaults to `THEKERNEL_MM_PERF_CPUS="4 8"`. Kernel build cache
identities include the CPU count from `THEKERNEL_KERNEL_CPUS` (or make's
`SMP`) and the complete maintained source trees used through Cargo path
dependencies. `THEKERNEL_QEMU_CPUS` controls the runner topology; the adapter
sets the product variables to the same value and the guest record provides the
third, runtime check. The rootfs is rematerialized through its content-addressed
builder so the guest helper cannot silently predate the checked-out source.

The default `THEKERNEL_MM_PERF_MEASUREMENT_MODE=product` uses the ordinary
`shell` kernel profile. That profile does not compile the MM lock or ASID switch
diagnostic features, does not issue diagnostic control commands, and records
`not-collected` for every diagnostic artifact manifest field. These are the
only bundles accepted by the regression comparator.

Lock and address-space-switch attribution are a separate, opt-in diagnostic
run:

```sh
THEKERNEL_MM_PERF_MEASUREMENT_MODE=diagnostic \
  scripts/ci/nightly/mm-performance.sh
```

Diagnostic mode selects the `mm-performance` kernel profile. Its hashed guest
command stream disables and resets both diagnostic surfaces, enables them, runs
the complete workload, disables them (with a bounded drain retry for MM locks),
and only then reads `/proc/mm_lock_stats`, `/proc/asid_switch_stats`, and
`/proc/pmu_capabilities`. The lock parser requires matching header/end control
state and publication sequence,
`enabled=0`, `resetting=0`, `active_samples=0`, complete unsaturated histograms,
nonzero samples in all six user-pin stages and physical publish/release, and at
least one exercised `mremap` stage. The ASID parser requires one disabled,
nonempty switch snapshot and emits a separate hashed TSV artifact. The PMU
parser requires one architecture-matched capability header and all five unique
typed event rows, with `samples_collected=0` and every `sampled=0`; it validates
the authoritative raw log but does not create or claim a measurement artifact.
Diagnostic counters can perturb the paths they measure, so diagnostic bundles
are deliberately rejected as product regression evidence instead of being
compared to product or to other diagnostic bundles.

The diagnostic profile is for attribution and distribution shape only. Its
`wait_ns` is lock acquisition latency, including fixed acquisition cost, not
pure contention time. Physical-shard sample publication currently runs while
the outer address-space guard is still held, so it can increase the reported
`user_pin_collect_owners` hold time. The collection window also spans the whole
helper, including semantic preflight, worker warmup, measured operations, and
cleanup; stage counts therefore do not map one-to-one to measured samples.
Only the runtime-off product profile is authoritative for performance values.

Before the matrix starts, the adapter intersects the runner's allowed affinity
with sysfs topology and groups CPUs by physical package and maximum frequency.
It selects one group large enough for the largest matrix entry and uses nested
subsets for smaller entries. If no such group exists, the adapter returns `78`
instead of mixing unlike cores. `THEKERNEL_MM_PERF_HOST_CPUS` may specify an
explicit pool, but that pool is still checked for one package/frequency class
and must be within the runner's inherited affinity. Each per-run shell applies
the selected affinity before entering both the kernel/rootfs build path and
QEMU, so every child inherits the same host CPU set.

`mm-performance.sh` refuses to label a dirty checkout as exact-HEAD evidence.
Its manifest records the full TheKernel, `thekernel-ax`, and
`thekernel-linux-abi` commits; workload parameters; guest online topology;
measurement mode and the derived kernel profile;
kernel and rootfs SHA-256; QEMU version and binary SHA-256; a runner and runner
contract fingerprint; the host CPU set, selection method, and class; and the
immutable per-run kernel, command, guest-input, QEMU-runner, metrics, log, and
pre/post host-diagnostic artifacts. The QEMU runner hashes and counts only bytes
successfully relayed into its stdin pipe, distinguishes producer EOF from a
broken pipe, and leaves the receipt awaiting the wrapper's real producer status.
The wrapper atomically finalizes that receipt only after the pipeline exits;
exact evidence requires a normal producer exit and an exact SHA-256, byte, and
line-count match with the unchanged staged command artifact. This proves relay
into the QEMU stdin pipe, while guest semantic and completion markers prove
execution. Host diagnostics are bounded to 64 KiB and contain only selected CPU
topology/frequency, load, CPU pressure, and cgroup CPU accounting; process names,
command lines, hostnames, and user identifiers are not captured. It rechecks
all three clean HEADs after the matrix. The guest
also completes fixed-destination replacement, shared `old_size == 0`
alias/coherence, and grow/shrink prefix-integrity checks before emitting the
semantic-pass marker.

The MM output is a `thekernel-mm-performance-bundle-v10` directory. Every QEMU
row is explicitly classified as `qemu-tcg` with `pmu_source=none`; CPU model,
firmware, and frequency-policy fields use `not-applicable`. The comparator
rejects physical or architectural-PMU claims until a separate physical receipt
authority exists. Manifest artifact paths are normalized POSIX paths relative
to the bundle root; every referenced artifact carries a SHA-256 and byte size.
The comparator rejects
absolute paths, `..`, symlink escapes, missing files, and digest or size drift.
It reruns the versioned performance parser on every raw QEMU log and requires
the result to be structurally identical to the per-run metric artifact. In
diagnostic mode it also requires the lock and ASID parsers' canonical outputs to
match their diagnostic artifacts byte for byte and reruns the capability-only
PMU parser against the raw log. A product log containing any `MM_LOCK_`,
`ASID_SWITCH_`, or `PMU_` record is invalid. An architecture-matched PMU
capability `source` says only which typed events the backend reports as
requestable. Because all capability records prove zero samples, they do not
change the QEMU-TCG manifest's `pmu_source=none` and are not PMU measurements.
The top-level metric matrix must also equal the union of the hashed per-run
metric files. These checks establish bundle-internal derivation,
not external authorship; publication that requires adversarial provenance still
needs an external signature or trusted transparency record. Copy the complete
directory, not individual files. Old v1
manifests containing `/workspace/...` paths, single-sample v2 bundles, v3
bundles without the three path-specific `mremap` metrics, and v4 bundles without
an explicit measurement boundary are intentionally rejected. V5 bundles lack
the independent-address-space pin workload, and v6 bundles do not bind the
guest-input and QEMU command-stream receipts. Older evidence is rejected rather
than guessed into a new provenance claim. V8 bundles do not carry the explicit
platform and PMU provenance fields and are rejected for the same reason.
V9 bundles lack the address-space-switch metric and the bound ASID and
capability-only PMU diagnostic contracts, so they are rejected rather than
upgraded by inference.

One adapter invocation captures one bundle and never labels a single sample as
a regression result. Capture adjacent, counterbalanced baseline/candidate pairs
on the same quiet runner (`B1 C1`, `C2 B2`, `B3 C3`, and so on), then pass at
least three pairs, in pair order, to the comparator:

```sh
scripts/ci/compare-mm-performance.py \
  --baseline /evidence/baseline-1 --candidate /evidence/candidate-1 \
  --baseline /evidence/baseline-2 --candidate /evidence/candidate-2 \
  --baseline /evidence/baseline-3 --candidate /evidence/candidate-3 \
  --policy scripts/ci/nightly/mm-performance-regression-policy.json \
  --stability-policy scripts/ci/nightly/mm-performance-stability-policy.json \
  --output /evidence/mm-regression.tsv
```

By default, every input bundle must contain exactly the release matrix
`rv4`, `rv8`, `la4`, and `la8`. `--allow-partial` accepts only a nonempty
subset for local triage, prints `PARTIAL`, and records `release_gate=false`;
its result is not publication evidence.

Qualify a runner and workload revision first with a counterbalanced null series
whose baseline and candidate sides use the same exact TheKernel commit. A null
series that returns `2` is evidence that the runner or workload is too noisy;
do not use that environment to accept or reject a kernel optimization. Keep the
failed bundle intact for diagnosis rather than adding pairs after existing
ratio extrema have already made the configured spread unattainable.

The pair count must be odd and within the independent, versioned stability
policy (three through nine by default). Every bundle is validated before use;
every pair must have matching workload, dependency, rootfs, QEMU, runner, host
CPU, and command provenance; and every bundle on one side must retain the same
TheKernel commit and per-run kernel hash. The comparator orders candidate over
baseline ratios using integer cross multiplication and gates their exact median,
without floating-point rounding.

CLI order alone is not evidence of paired capture. Each hashed host diagnostic
contains a strict UTC RFC3339 timestamp. The comparator forms a pre/post capture
interval for every run, rejects reversed or overlapping intervals and duplicate
bundle receipts, and requires each pair to contain two disjoint adjacent
captures while pair orientation alternates baseline-first, candidate-first,
baseline-first (or the reverse). Pair intervals must also be chronological and
disjoint. Equal interval boundaries are rejected, so copied or concurrently
captured inputs cannot count as independent counterbalanced pairs.

The repository regression policy gates every metric's median P99 at no more
than 20 percent latency regression and requires all three direct-I/O proxy metrics to retain at
least 90 percent of baseline throughput. A custom policy may be stricter, but
the validator rejects any policy that weakens either limit. The independent
stability policy rejects a gated statistic when its maximum paired ratio is
more than 20 percent above its minimum paired ratio. That is noisy evidence,
so the comparator returns `2`; it does not convert uncertainty into a pass or a
regression.

Every P999 value is compared and reported as `REPORT_ONLY`, which is not a pass
and cannot be enabled as a hard gate by policy. A single run currently gives
only 64 direct-I/O samples and at most 512 samples for the other default metric shapes; its P999
is too close to a maximum sample to serve as a stable hard gate under QEMU.
Future hard P999 gating requires a new evidence and policy schema after enough
paired samples justify it.

The comparator refuses a conclusion unless baseline and candidate have the
same run-key set, online CPU topology, workload and sample counts, maintained
dependency commits, rootfs, guest command, QEMU binary/version, runner
fingerprint, and runner contract. TheKernel commit and kernel artifact hashes
may differ: those are the product under comparison. Automatic runner identity
includes stable host/CPU and cgroup allocation facts. A controlled runner can
set `THEKERNEL_MM_PERF_RUNNER_ID` to a durable declared identity, but doing so
is an operator assertion that the underlying performance environment remains
equivalent, not a general bypass for comparing unrelated machines.

Each architecture/CPU cell finishes its content-addressed kernel and rootfs
build before the host capture interval. The rootfs identity covers the guest
helper sources and compiler identity, so fixing the rootfs digest also fixes
the compiled MM helper consumed by that cell. The adapter stages the kernel,
command stream, and input receipts, then waits a bounded quiet period before
capturing `host-pre.tsv`. `THEKERNEL_MM_PERF_SETTLE_SECS` defaults to 5 seconds,
accepts integers from 0 through 60, and participates in the runner-contract
digest; unit tests may set it to zero. The measured executor only consumes the
prepared artifacts with both rebuild flags disabled. After `host-post.tsv` is
captured, the adapter fail-stops if the staged kernel, source rootfs, or their
input receipt hashes drifted.

It returns `0` when every enabled gate passes, `1` for a measured regression,
and `2` when an input is invalid or not comparable, or when the paired series
is noisy. This is a repeatable QEMU
regression-triage contract, not a production latency SLO or proof of one
internal lock's isolated cost.

Rootfs byte reproducibility is checked separately with two fresh source caches,
compiler work directories, staging trees, and output paths per architecture:

```sh
scripts/ci/check-rootfs-reproducibility.sh \
  --arch both --workdir .state/ci/rootfs-reproducibility
```

The image helper normalizes the staging tree, fixes UUID/hash seed and lazy
initialization, uses `E2FSPROGS_FAKE_TIME` for ext4 creation clocks, then asks
libext2fs to canonicalize every imported inode's atime, ctime, mtime, crtime,
and checksum. The gate requires equal full-image sizes, SHA-256 digests, and
byte content.

`smp-tlb-shootdown.sh` applies the same clean-source rule to TheKernel,
`thekernel-ax`, and `thekernel-linux-abi`, forces a topology-specific kernel and
content-addressed rootfs rebuild for every matrix cell, and rechecks all three
repositories afterward. Before QEMU starts, every attempted cell stages the
exact kernel and command stream, a rootfs digest sidecar, and a structured input
receipt containing artifact sizes and hashes plus QEMU, cross-compiler, Rust,
and Cargo identities. The shared QEMU runner atomically records its resolved
binary, complete argument vector, input hashes, lifecycle result, duration, and
error in JSON. These per-cell receipts survive boot failures; the matrix
manifest remains stricter and contains only semantically validated cells. A
complete manifest records the three commits, requested and guest-observed
topology, the staged artifact and tool hashes, and paths to every receipt and
console log. A separate provenance receipt records the exact three repository
commits at both preflight and finalize. The rootfs sidecar is a digest receipt
for the content-addressed, repository-built image, not an embedded copy of the
96 MiB sparse filesystem.
