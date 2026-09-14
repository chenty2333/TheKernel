# Running a compiler and an emulator inside TheKernel userspace

Status: design record; the proposed guest toolchain and nested-QEMU integration
are **not implemented or guest-validated**. This revision corrects the plan,
not the kernel. Its source references describe the revision by `edcff91a`;
the plan is now carried out on branch `feat/guest-toolchain` in the worktree
`/home/ava/Worktrees/TheKernel/guest-toolchain`, forked from `dev` at
`44621445`. §2's baseline gate has since been satisfied on that branch (§2,
last row); nothing else in the plan has been implemented.
Existing code, old run observations, upstream behaviour and proposed work are
distinguished below; no VM was started during this document revision. This
revision adds git-history archaeology for §2 (inspection only, no VM runs),
selects Alpine's `linux-virt` kernel and minirootfs as the first nested
Linux guest, and commits to a dynamic-glibc acceptance milestone ahead of
Phase 3.

The immediate goals are native C compilation and a nested Linux boot.
Running TheKernel's own acceptance image inside that nested machine is a
separate later target, not a claim established by either immediate goal.
The first nested Linux guest is an Alpine userland: still a real Linux
kernel, but deliberately not a full desktop distribution, narrowing what the
nested boot proves.

## 1. Emulator mode is not execution privilege

**Both the compiler and QEMU remain ordinary TheKernel userspace processes.**
QEMU's *user-mode* and *system-mode* describe what is emulated, not whether the
QEMU process runs in userspace or kernel space.

* `qemu-x86_64 ./program` is user-mode emulation: it executes one program and
  translates its Linux syscalls and signals to the hosting OS. There is no
  second kernel. This is **not a prerequisite** for system-mode QEMU.
* `qemu-system-x86_64 -accel tcg …` emulates a complete machine, including the
  CPU, RAM and devices needed to boot another kernel. QEMU itself still uses
  ordinary TheKernel syscalls for memory, threads and I/O.

The preferred development topology is:

```text
Host Linux + outer QEMU/KVM
└── TheKernel kernel
    └── TheKernel userspace: qemu-system-x86_64 / TCG
        └── Inner x86_64 machine
            ├── Linux kernel
            └── Inner test program
```

Outer KVM plus inner TCG is not double TCG. Outer TCG plus inner TCG is a
different, slower topology to measure separately. No guest `/dev/kvm`,
kernel-resident QEMU, or nested hardware-virtualization work is planned.

| Target | Concrete result | Boundary |
|---|---|---|
| Native C compilation | `tcc -static hello.c -o hello && ./hello` inside TheKernel, using the installed sysroot | Compiles and executes a native ELF; not compiler self-hosting |
| Nested Linux boot | Static-musl `qemu-system-x86_64` boots Alpine's `linux-virt` kernel and minirootfs via direct kernel boot, using TCG | Proves a real distribution kernel and musl/BusyBox userland in the nested machine; not a full desktop Linux, not TheKernel's own acceptance |
| Later: nested TheKernel acceptance | The same emulator runs the project's x86_64 q35/UEFI acceptance image | Needs the matching firmware, devices and test control; does not imply rebuilding TheKernel inside itself |

Do not add a user-mode port or a hand-written TCG machine as mandatory stages.
User-mode has distinct syscall/signal translation contracts; system-mode's
ability to load an ELF does not supply an ordinary Linux executable's OS
environment. Use an existing machine and supported boot path.
See [QEMU user-mode documentation](https://www.qemu.org/docs/master/user/main.html)
and [Generic Loader](https://www.qemu.org/docs/master/system/generic-loader.html).

## 2. Existing facts and the explained baseline

| Observation | Source and limit |
|---|---|
| Host tools are available | Reviewed host: GCC 16.2.1, clang 22.1.8, QEMU 10.2.2; accelerator help includes `tcg` and `kvm`. These are not guest-tool version selections. |
| Kernel C and guest C have different build paths | `crates/ax/thekernel-lwext4-rust/build.rs::discover_llvm_freestanding_toolchain` selects clang for the freestanding C component. `scripts/build-rootfs.sh` uses the selected host/cross GCC to statically build BusyBox, init and guest helpers. |
| The default rootfs is 96 MiB | `scripts/build-rootfs.sh`: `SIZE_MB=96`; `--size-mb` already exists. |
| The current system suite has 42 cases | `tests/guest/system-init.c::suite`; `io-uring-directio` is case 27. |
| An existing daily log reports success | `~/.cache/thekernel-targets/dev/accept-daily.log` contains `1..42`, case 27 `result=0`, `guest-tcg: PASS` and `daily: PASS`. Reading this log is not a fresh acceptance run. |

The previous draft described an ad-hoc OVMF/q35 TCG boot at 512 MiB / 4 CPUs
with **41 cases**, reporting `not ok 26 - io-uring-directio` and
`THEKERNEL_IO_URING_DIRECTIO_FAIL stage=close-pending-reregister errno=16`.
A 100-second host timeout killed that run before normal guest shutdown.

That case count and ordinal do not match the current source, and git history
explains why. At `ca428bda` the compiled suite had exactly **41 cases** with
`io-uring-directio` at **ordinal 26** — matching the old report precisely.
The next suite change, `2cdce873`, inserted `exit-status-trace-read` ahead of
it, producing the current 42 cases at ordinal 27. The old run was therefore a
valid run of a `ca428bda`-era image that the previous draft mis-attributed to
its tip: version drift, not a record, artifact or QMP/wiring anomaly. The one
question the record cannot answer is whether the `errno=16` (EBUSY) I/O
failure still exists at the current tip.

**Before adding guest functionality**, rerun the official guest runner with
the original TCG / 512 MiB / 4-CPU parameters, rebuilding through that entry
point (§9). This is now a routine confirmation, not an investigation: require
the current suite, no failures or skips, and normal shutdown. If the I/O
failure recurs, resolve it before extending the baseline.

That confirmation was completed on branch `feat/guest-toolchain` at
`44621445`, built and booted through `./tools/thekernel.py test --suite guest`
(§9). Result: `1..42`, **42 `ok`, 0 `not ok`, 0 skips**,
`# THEKERNEL_SYSTEM_TEST_COMPLETE`, then `/bin/busybox poweroff -f` and
`qemu-runner exit=0`. `io-uring-directio` ran at ordinal 27 with
`result=0`; the `errno=16` completion observed in the old
`ca428bda`-era boot did **not** reproduce. Do not carry the old failure
forward as an open kernel defect: it is explained as version drift and now
also unobserved at the current tip. The ordinary warning lines in that run
(a declined boot framebuffer under `-display none`, an invalid
`rt_sigreturn` frame and an invalid syscall number probed deliberately by
existing cases) are case behaviour, not new regressions.

## 3. Existing ABI mechanisms, not blanket compatibility guarantees

1. **Routing is not semantic coverage.** `config/linux-abi.toml` inventories
   385 routes against Linux v7.2.3: 366 ordinary handlers and 19 explicit
   `ENOSYS` routes. Its header explicitly disclaims implementation evidence.
   The refused calls are not all legacy: `config/linux-contracts.toml` names
   470 as `listns` and 471 as `rseq_slice_yield`. Neither this count nor a
   successful dispatch proves a compiler/QEMU contract.
2. **Executable mappings have implementation support.**
   `kernel/src/syscall/mm/mmap.rs::mdwe_refuses_execute_gain` gates MDWE
   restrictions on process opt-in. `may_protect_from_file_flags` starts with
   READ and EXECUTE, adding WRITE for writable files; execution is not
   conditional on the file being writable. These paths justify testing JIT
   memory, not claiming that its full lifetime already works for QEMU.
3. **memfd support is narrower than Linux.**
   `kernel/src/file/memfd.rs::MEMFD_SUPPORTED_CREATE_FLAGS` accepts
   `MFD_CLOEXEC | MFD_ALLOW_SEALING`; `MFD_EXEC` and `MFD_NOEXEC_SEAL`
   are rejected with `EINVAL`. That is not automatically a workload blocker.
   The reviewed [QEMU 10.2.2 memfd helper](https://github.com/qemu/qemu/blob/v10.2.2/util/memfd.c)
   does not request `MFD_EXEC`; recheck the version and configuration actually
   selected for delivery.
4. **There is no guest KVM interface.**
   `kernel/src/pseudofs/dev/mod.rs` provides the synthesized devices, not
   `/dev/kvm`. Use TCG inside TheKernel; host KVM does not expose its device
   automatically to the guest.
5. **Procfs has gaps.** `kernel/src/pseudofs/proc.rs` includes `cpuinfo`,
   process `maps`/`status` and `/proc/sys/vm/*`, but no `auxv` entry.
   Determine which paths the selected binaries actually require and which
   failures they handle; do not make every Linux procfs path a prerequisite.
6. **Reuse the current test machinery.** The `system-init.c` case table emits
   KTAP plus `THEKERNEL_TEST_BEGIN/END`. Existing coverage includes
   `tests/guest/tools/signal-fp-smoke.c` (nested signals, alternate stack and
   FP restoration), futex helpers, and
   `tests/guest/portable/mm-contracts-differential.c` in the ABI differential
   machinery. Extend only missing workload-relevant contracts.
7. **The ELF loader supports `PT_INTERP`.** `kernel/src/mm/loader.rs` resolves
   the `Type::Interp` program header, maps the dynamic linker at
   `USER_INTERP_BASE` and starts at the linker's entry, with credential checks
   across the shebang/interpreter chain. Loader support is not glibc
   compatibility: ld.so initialisation, TLS setup, IFUNC resolution and the
   fallback paths of the syscalls glibc issues early (`rseq`, `clone3`,
   `statx`, `getrandom` — all routed per `config/linux-abi.toml`) remain
   unvalidated. This is the entry point for the Phase 3 dynamic-glibc
   milestone (§5), not evidence that dynamic toolchains work today.

## 4. Packaging decision

**Build the guest tools on the host, stage their complete runtime/development
inputs, and install them as an optional rootfs payload.** Guest acceptance
must not download packages or depend on guest networking.

* Use static `x86_64-linux-musl` tools initially to limit runtime-library
  dependencies. This does not eliminate every `/etc` dependency, and static
  tool binaries do not imply static compiler output. The existing
  `THEKERNEL_USE_LOCAL_MUSL` option is a host-build mechanism, not a packaged
  native guest compiler. Static glibc was considered and rejected: its NSS
  architecture (`getpwnam`/`getaddrinfo`) still `dlopen`s `libnss_*.so` at
  run time, so "static" glibc binaries keep hidden runtime library
  requirements and fail subtly. Dynamic glibc is a deliberate Phase 3
  milestone instead (§5).
* Stage under `/opt/thekernel-tools/` and cache builds under
  `THEKERNEL_STATE_DIR` (default `~/.cache/thekernel-targets`). Keep sources,
  build trees, images and firmware VARS on storage under `/home`, never on the
  host's RAM-backed `/tmp` or `/dev/shm`.
* Pin every source version before building, and record the pins as
  fingerprint inputs (§6): the tcc `mob` commit, the QEMU release, the musl
  release and the Alpine release. Build the musl sysroot from that pinned
  musl source with the host cross toolchain — staging host-built artifacts is
  expected, downloading binaries into the guest is not. Follow existing
  repository build conventions; do not add a package manager or general
  toolchain-management framework.
* First run the staged tools and intended workload on host Linux with the
  staged include/library/data paths. Check x86_64 ELF type, interpreter and
  dynamic dependencies for all executables. This catches packaging problems;
  it does not establish TheKernel compatibility or byte-for-byte reproducibility.

Guest source bootstrap is deferred until a working compiler has a real use.
Buildroot's host SDK is not a ready-made native guest GCC toolchain: its
[FAQ](https://buildroot.org/downloads/manual/manual.html#faq-no-compiler-on-target)
states that target-native compiler support was removed. Existing Buildroot
infrastructure can still build suitable target packages, but the desktop
rootfs is not a fallback that automatically supplies this development setup.

## 5. Implementation order and acceptance

### Phase 0 — Confirm the baseline, then probe actual dependencies

First complete §2's baseline gate. For each subsequent tool, select and
host-check its build before its guest experiment. Inspect upstream paths and
try the real binary, then use focused probes to isolate actual failures.

| Contract | Probe or reuse | Required interpretation |
|---|---|---|
| Executable memory | Anonymous executable mappings and the selected JIT's permission transitions, execution, reuse and teardown | Test the real allocation strategy, not only a one-shot RW→RX call |
| Split-WX mappings, if selected | Shared RW/RX aliases with writes visible to execution | memfd flag success is not a substitute for alias correctness |
| Signals and FP state | Reuse `signal-fp-smoke.c`; extend fault delivery/context restoration only where needed | user-mode and system-mode signal paths are not interchangeable |
| Threads and waiting | Exercise the chosen musl's `pthread_create` path and reuse futex tests | Do not assume raw `clone3` is that libc's required creation path |
| Procfs and address placement | Check actual accesses; extend existing mmap tests where the consumer needs them | QEMU `-m` selects inner RAM size, not a `MAP_32BIT` host-address requirement |

Each check must distinguish **required success**, **an allowed failure with a
working application fallback**, and **an informational observation**. For
example, unsupported `MFD_EXEC` or missing `auxv` must not become blanket
blockers unless the chosen workload needs them. No arbitrary six-probe
all-success gate or half-day completion promise is made.

A required failure blocks the affected capability, not an unrelated one.
Make only the needed kernel fix and regression test before resuming it.
Use existing KTAP reporting; diagnostics should identify the operation,
result/errno and failed semantic condition.

### Phase 1 — Native C compilation with tcc

* Build static tcc for execution on `x86_64-linux-musl`. Install tcc's internal
  headers and `libtcc1.a`, plus a matching musl development sysroot: system
  headers, `libc.a` and required startup objects such as `crt1.o`, `crti.o`
  and `crtn.o`. Configure the guest include/library search paths.
* Add `compiler-smoke`: write a small C program using a standard header and
  libc to a guest scratch directory, compile it with explicit `-static`,
  then execute the resulting ELF in a new process. Check compiler status,
  program output and program exit status separately; retain diagnostics in
  the ordinary test output on failure.
* Select the optional tcc payload through the existing build/test entry
  points (§6); size that image for its actual installed contents. The default
  96 MiB image and baseline suite must not require tcc.

Acceptance: `compiler-smoke` and all selected guest cases pass, with complete
KTAP and normal shutdown. Host-side compilation alone does not satisfy it.
[TCC's linker documentation](https://www.bellard.org/tcc/tcc-doc.html) distinguishes
static output from the default dynamically linked executable.

Treat `tcc -run` as a separate follow-up case, not a Phase-1 prerequisite.
The reviewed [upstream runtime](https://github.com/TinyCC/tinycc/blob/mob/tccrun.c)
uses in-memory relocation and permission changes; memfd is not an inherent
requirement. Validate the selected static-musl build's symbol resolution and
runtime support rather than assuming that ordinary ELF output proves `-run`.

### Phase 2 — Nested Linux on x86_64 system QEMU / TCG

* Build `qemu-system-x86_64` as a static musl executable with a small, explicit
  feature set. Include the static dependencies and firmware/data required by
  the chosen machine. Avoid desktop display, audio, networking and unused
  backends for this first serial-console workload. **The static-musl QEMU
  build is the largest single build risk in this plan**: QEMU's build system
  leans on glibc assumptions, so validate the host build before any guest
  experiment, using Alpine's qemu aport as the reference for musl-specific
  patches and configure options.
* Boot the inner machine with `-kernel`/`-initrd`/`-append`, skipping firmware
  entirely — firmware POST under TCG is a significant share of boot time.
  Use the `pc`/`q35` machine by default so ACPI poweroff satisfies the
  normal-shutdown condition below. Consider `microvm` only after verifying
  that its optional ACPI provides a working S5 path on the pinned QEMU
  version; do not assume it.
* Stage the boot in three rungs, each with its own acceptance:
  1. **Freestanding hello kernel (Phase 2a).** A minimal `-kernel`-loadable
     ELF that prints a serial banner and exits QEMU through `isa-debug-exit`
     with a fixed result code. Boots in seconds and isolates "TCG works in
     TheKernel userspace" from "Linux boot is slow". Add guest case
     `nested-tcg-hello` for it.
  2. **Alpine Linux (Phase 2b — the nested-Linux acceptance target).** A
     pinned Alpine release: the `linux-virt` kernel flavour plus the
     minirootfs (musl + BusyBox), staged from host-downloaded release
     artifacts. This narrows the claim from "a full desktop Linux" to "a
     real, widely used distribution kernel and userland"; the narrowing is
     accepted. No `apk`, no guest networking. A prebuilt VM-oriented kernel
     is preferred here over maintaining a custom config from the start.
  3. **Custom trimmed kernel (Phase 2c, conditional).** Only if the measured
     Phase 2b boot time breaks the runner timeout budget. Build up from
     `tinyconfig`, not down from `defconfig`: no modules, serial plus the
     required virtio drivers only, `init_on_alloc`/`init_on_free` off, no
     debug options, an LZ4 or uncompressed image, and a command line with
     `quiet`, a fixed `lpj=`, `tsc=reliable` and `mitigations=off`.
* Report inner results as prefixed serial markers — an `INNER_`-prefixed
  variant of the existing `THEKERNEL_TEST_*` style — so the existing runner
  parsing and timeout handling can be reused. Reserve `isa-debug-exit` for
  the Phase 2a result channel and as an inner-initiated abort path: it
  encodes a result in QEMU's exit status but is a forced exit, not a normal
  shutdown, and therefore cannot substitute for the shutdown condition below.
* First boot those same staged components on host Linux using **TCG**, then
  run them inside TheKernel. Start with one inner vCPU and
  `-accel tcg,thread=single`; explicitly select the inner RAM size, machine,
  code-cache size (`tb-size`) and supported executable-memory mode.
* Use outer KVM for initial development. **4 GiB outer RAM is a conservative
  starting budget, not a measured minimum.** Account for outer kernel/rootfs,
  QEMU, its code cache and inner RAM. The current guest suite uses module
  rootfs transport, which must also be included in the memory assessment.
  Measure the Alpine boot time and footprint before setting acceptance
  timeouts; if they fit the budget, Phase 2c is not needed.

QEMU exposes cache and split-WX controls; the reviewed
[10.2.2 TCG allocator](https://github.com/qemu/qemu/blob/v10.2.2/tcg/region.c)
has both anonymous and split-WX paths. Pin the selected configuration and
probe its actual allocation/protection sequence instead of requiring a
hypothetical `MFD_EXEC` path.

Add `nested-linux-boot` (Phase 2b) with four distinct conditions:

1. The inner workload completes its checks and reports an explicit success
   result through the serial channel as `INNER_`-prefixed markers.
2. The inner OS performs normal shutdown, not panic, reset or forced timeout.
3. The outer test waits for the QEMU process and checks its exit status.
4. The outer guest completes its own selected KTAP suite and normal shutdown.

QEMU exit status zero alone does not prove inner test success. Bound the inner
wait and outer runner timeout, reap the emulator on failure, and classify
forced termination as failure. Keep inner output separately prefixed/captured
so it cannot be mistaken for outer KTAP. No kernel-space emulator is involved.

### Phase 3 — A larger compiler only for a concrete consumer

Introduce GCC or Clang when a real guest build needs it, not just to add another
compiler name. A GCC payload needs the driver, `cc1`, assembler/linker,
matching compiler runtime, libc, startup objects and headers. Clang needs its
frontend, linker and matching builtins/sysroot. Static-linking only a driver
is insufficient: every invoked executable must run with the installed payload.

Add a consumer-representative case, such as compiling and executing a
multi-file program using standard headers and `pthread`, through the same
selection and KTAP mechanisms.

**Dynamic glibc is a committed prerequisite milestone for distro-built
GCC/Clang payloads; the expanded ABI surface is accepted.** Distribution
toolchains are overwhelmingly glibc-dynamic, and that form — not static
glibc — is the one worth supporting. The loader already handles `PT_INTERP`
(§3 item 7), but before selecting any glibc toolchain, run Phase-0-style
probes over the full startup chain: ld.so initialisation, TLS setup, IFUNC
resolution, and the fallback paths of `rseq`, `clone3`, `statx` and
`getrandom`. Fix and regression-test whatever those probes expose, then gate
the GCC/Clang payload on a dynamic-glibc smoke case. musl remains the
Phase 1/2 vehicle; this milestone gates Phase 3 rather than replacing it.

Keep the claims separate: compiling hello is native compilation; guest GCC
rebuilding tcc is a guest source build; tcc rebuilding itself can demonstrate
compiler self-hosting if the rebuilt compiler is also tested. Rebuilding
TheKernel additionally requires Rust/Cargo, freestanding LLVM and image-build
tools and is outside this plan. Nested TheKernel acceptance can instead use
host-built images, but still needs its own later end-to-end gate.

## 6. Minimal integration with existing entry points

Use one explicit payload selection, proposed as
`--toolchain {none,tcc,nested}`, default `none`; `nested` includes tcc and
the nested-Linux components (QEMU plus the staged Alpine artifacts). **This
flag is not implemented.** Extend the existing CLI, not a parallel command or
general profile framework. The name `nested` describes the workload rather
than listing tools, so a later `gcc` payload — gated on the Phase 3
dynamic-glibc milestone — extends the enum without redesigning it.

| Concern | Existing mechanism | Required change |
|---|---|---|
| Build selection | `tools/thekernel.py::build_rootfs` calls `scripts/build-rootfs.sh` | Pass the same selection through build and test; install only that payload and choose sufficient image size |
| Artifact identity | `Artifacts.rootfs` currently returns one shared image path | Distinguish selected rootfs outputs and rootfs-dependent kernel/ESP artifacts so the tool image cannot replace the default image |
| Rebuild decisions | `rootfs_fingerprint()`, input files/globs and environment | Include payload selection, source definitions, libc/toolchain inputs and build options; hashing only a new builder script is insufficient |
| Build cache | Existing state directory and build serialization | Reuse them; key cached tools by relevant versions/options/toolchain, not host architecture alone |
| Case selection | `system-init.c` compiled by the rootfs builder | Include compiler cases for tool payloads and nested cases (`nested-tcg-hello`, `nested-linux-boot`) only for `nested`; compute KTAP's plan from that table |
| Runtime resources | Existing `--memory`, `--timeout` and runner | Choose explicit per-workload budgets from measurement; preserve ordinary defaults |

Default `none` keeps the 96 MiB baseline and its existing case table. Selecting
a tool payload makes its cases mandatory: a missing binary is a failure, not a
skip or automatic case omission. Compiler `-run` remains outside the required
Phase-1 gate until separately validated.

Initially keep tools inside the selected rootfs. Consider a second disk only
when measured image size or boot-memory cost justifies it. `--extra-block`
exists for the run path, but `system_test_cmd` currently passes
`extra_block=None`; guest-test attachment, mounting and read-only usage would
still need deliberate integration. No second-disk workflow is added now.

## 7. Risks and stop conditions

* The §2 baseline mismatch is explained as version drift, but the
  confirmation rerun must still complete before attributing new guest
  failures.
* The static-musl QEMU build is the plan's largest single build risk.
  Host-validate it first, with Alpine's qemu aport as the musl patch
  reference; a failure here reopens the toolchain selection, not the kernel.
* The Alpine guest narrows the nested-boot claim (real distribution kernel
  and musl/BusyBox userland, not a full desktop Linux). The narrowing is
  accepted; nested TheKernel acceptance remains a separate later target.
* The committed dynamic-glibc milestone expands the ABI surface under test.
  Contain it behind the Phase 3 gate with Phase-0-style probes; do not let
  glibc runtime fixes bleed into the musl-based Phase 1/2 work.
* Host success, route coverage and focused probes do not prove application
  compatibility. Repair demonstrated contracts, not an imagined complete ABI.
* A selected tool's unsupported behaviour or static dependency can block that
  capability. Narrow the workload before adding infrastructure; keep already
  working native compilation independently usable if nested boot stalls.
* No artifact size, minimum RAM or nested boot duration has been measured.
  Measure the selected topology; double-TCG timing is not a KVM/TCG estimate.
* Larger images can raise boot-memory cost as well as disk usage. Revisit the
  transport only when actual measurements justify it.
* Guest KVM, other TheKernel architectures, custom TCG machines and full OS
  source bootstrap remain out of scope.

## 8. Decisions retained after review

1. tcc first, static musl, with a complete development sysroot, pinned source
   versions and explicit static ELF output.
2. Directly target x86_64 **system-mode QEMU in TheKernel userspace**; no
   required user-mode port and no custom emulator machine.
3. Nested guest ladder: freestanding hello kernel (Phase 2a), then Alpine
   `linux-virt` + minirootfs via direct kernel boot as the nested-Linux
   acceptance target (Phase 2b); a custom trimmed kernel only if measured
   boot time requires it (Phase 2c). `pc`/`q35` by default for ACPI
   poweroff; `isa-debug-exit` as the early-stage result and abort channel.
4. musl static for Phase 1/2 tools; a dynamic-glibc acceptance milestone is
   committed as the Phase 3 prerequisite for distro-built GCC/Clang payloads.
5. Optional rootfs payload first (`--toolchain {none,tcc,nested}`); a second
   disk only if measured costs require it.
6. Select versions/options before their host smoke and guest probes; defer
   nested TheKernel acceptance until its consumers are explicit.

## 9. Baseline verification commands (existing interfaces only)

These are the commands the baseline gate was completed with on branch
`feat/guest-toolchain` (result in §2). They remain reproducible as written;
keep them as the reference invocation for later guest work. The existing test
entry point rebuilds unless told otherwise; do not use `--no-build` here — the
rebuild through the official entry point is exactly what confirms the current
source produces the current case table. On this Fedora host the builder falls
back to the native GCC (`x86_64-linux-gnu-gcc` does not exist and
`gcc -static` does), so the image's BusyBox and helpers are **static glibc**,
not musl; the guest payloads this plan adds are built from pinned musl sources
(§4) rather than by changing that fallback.

```bash
cd /home/ava/Worktrees/TheKernel/guest-toolchain

# Host observations only; not evidence that these binaries work in TheKernel.
gcc --version
clang --version
qemu-system-x86_64 --version
qemu-system-x86_64 -accel help

# Keep generated state on persistent storage and isolate this worktree's outputs.
source .gt-env.sh          # THEKERNEL_STATE_DIR + CARGO_BUILD_JOBS
./tools/thekernel.py test --suite guest --accel tcg \
  --smp 4 --memory 512M --timeout 300
```

At the cited base, require 42 cases, `io-uring-directio` at ordinal 27, no
failures/skips, the system completion marker and normal guest shutdown. The
case arithmetic is verified against git history (§2); this run confirms the
I/O failure is gone at the current tip, nothing more. A runner timeout is not
success; diagnose it rather than inferring acceptance.

Use the runner's firmware handling rather than the old ad-hoc boot command.
Any manual OVMF VARS copy or other VM image must also live under `/home`,
never host `/tmp`. Small source/output files in the **guest's** scratch
directory are a separate matter and may be used by `compiler-smoke`.
