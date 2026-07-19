# RFC 0006: Bounded Task-Local Seccomp Contract

- Status: draft
- Date: 2026-07-19
- Last implementation audit: 2026-07-19
- Owners: TheKernel maintainers
- Target layers: `thekernel-axcbpf`, `thekernel-linux-seccomp`, and the
  TheKernel syscall, task, credential, signal, process-exit, procfs, and CI
  adapters

## Problem

Seccomp is not only a classic-BPF verifier or a syscall-number switch. A Linux
consumer must copy and validate an untrusted program, publish an irreversible
task state, evaluate it before every syscall fast path, preserve that state
across clone and exec, select one action across an immutable ancestry, and
terminate or signal with Linux-visible register and thread-group semantics.

The resource and concurrency boundaries are equally important:

- a filter program and the complete inherited path must be bounded
  independently;
- failed verification, path admission, allocation, or publication must not
  change the live task;
- clone and fork must share immutable programs without duplicating their
  charge, while later task-local appends remain isolated;
- an exited task must release its task-local filter ownership even if a proc
  handle, pidfd, joiner, or scheduler handoff retains the task object;
- TSYNC cannot mutate a partially enumerated thread group;
- TRACE, USER_NOTIF, audit logging, and fatal core dumps each require an
  external lifecycle which is not supplied by recognizing the corresponding
  action number; and
- JIT implementation details must not accidentally become an undocumented
  resource contract.

Putting all of this in a generic BPF crate would make that mechanism depend on
Linux tasks, credentials, signals, errno, and UAPI. Putting the verifier and
immutable state directly in the syscall adapter would make reuse, independent
testing, and allocation boundaries difficult to audit. This RFC freezes the
three-layer ownership split and the deliberately bounded consumer profile.

## Evidence reviewed

The observable contract was checked on 2026-07-19 against the Linux `v6.12`
tag, commit `adc218676eef25575469234709c2d87185ca223a`, especially:

- [`include/uapi/linux/seccomp.h`](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/seccomp.h),
  [`filter.h`](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/filter.h),
  and [`audit.h`](https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/audit.h)
  for operations, flags, actions, the 64-byte input object, classic-BPF
  layout, and audit architecture values;
- [`kernel/seccomp.c`](https://github.com/torvalds/linux/blob/v6.12/kernel/seccomp.c)
  for install validation and permission order, immutable filter stacking,
  the 4096/32768 limits, action precedence, strict mode, clone/exec/exit
  lifetime, TSYNC admission, ptrace, notification, forced SIGSYS, and KILL
  behavior;
- [`net/core/filter.c`](https://github.com/torvalds/linux/blob/v6.12/net/core/filter.c)
  for classic-BPF validation and the cBPF-to-eBPF migration length retained in
  `bpf_prog::len` before seccomp path admission;
- [`kernel/bpf/core.c`](https://github.com/torvalds/linux/blob/v6.12/kernel/bpf/core.c)
  for constant blinding, runtime/JIT selection, native image accounting, and
  the distinction between path length and executable-memory limits;
- the RISC-V and LoongArch
  [`syscall_rollback`](https://github.com/torvalds/linux/blob/v6.12/arch/riscv/include/asm/syscall.h)
  implementations for architecture syscall-frame restoration; and
- the official
  [`seccomp_bpf` selftest](https://github.com/torvalds/linux/blob/v6.12/tools/testing/selftests/seccomp/seccomp_bpf.c)
  for raw error ordering, filter chains, TRAP, action precedence,
  KILL_THREAD/KILL_PROCESS scope, inheritance, TSYNC, flags, and notification
  behavior.

The Linux user-facing overview in
[`Documentation/userspace-api/seccomp_filter.rst`](https://docs.kernel.org/6.12/userspace-api/seccomp_filter.html)
was used to cross-check the intended API narrative. Repository-specific
provenance, reviewed commits, and conversion notes are also recorded in
`thekernel-linux-seccomp`'s `VENDOR.md` and `PATCHES.md`.

Linux implementation source is GPL-2.0-only. Linux UAPI headers carry their
own syscall-note license expressions. TheKernel, `thekernel-axcbpf`, and
`thekernel-linux-seccomp` reimplement public layouts, observable behavior,
arithmetic rules, and general ownership ideas in Rust; no Linux implementation
source was copied.

## Decision

### 1. Keep three explicit ownership layers

| Layer | Owner | Contract |
| --- | --- | --- |
| Layer 1 | `thekernel-axcbpf` | `no_std`, unsafe-free, policy-neutral classic-BPF instruction representation, structural verifier, immutable program storage, A/X/16-word scratch interpreter, and the input trait. It contains no seccomp action, syscall, task, credential, signal, errno, audit, socket, or FD policy. |
| Layer 2 | `thekernel-linux-seccomp` | `no_std`, `forbid(unsafe_code)` Linux v6.12 seccomp input/opcode profile, 64-byte `seccomp_data`, RV64/LoongArch64 audit values, converted path charge, immutable ancestry, logical live-byte accounting, action selection, irreversible task-state plans, and non-mutating per-sibling TSYNC eligibility. It receives already copied values and has no implicit current task or userspace pointer. |
| Layer 3 | TheKernel | `sock_fprog` and action-query usercopy, validation/permission order, exact credential snapshot, task publication lock, syscall-entry ordering, clone/fork inheritance, exec preservation, exit retirement, architecture frame access, errno mapping, SIGSYS and group exit, bounded diagnostics, procfs reporting, and guest composition. |

Layer 2 may describe a Linux action or return a TSYNC eligibility result
without claiming that Layer 3 exposes the corresponding lifecycle. A UAPI
constant, enum variant, or policy planner is not a support advertisement.

The existing eBPF subsystem is not reused as the classic-BPF verifier. Its
program type, verifier, maps, helpers, JIT expectations, and attachment
lifecycle are different ownership domains.

### 2. Freeze the 64-bit task-local ABI profile

The current consumer supports native 64-bit RISC-V and LoongArch ABIs. The
host x86_64 path exists for adapter compilation and tests; it is not a released
bare-metal architecture claim. There is no 32-bit compat `sock_fprog` profile.

The exposed operations are:

- `seccomp(SECCOMP_SET_MODE_STRICT, flags, uargs)`;
- `seccomp(SECCOMP_SET_MODE_FILTER, flags, uargs)` with `flags == 0`;
- `seccomp(SECCOMP_GET_ACTION_AVAIL, 0, action)` for actions implemented by
  this adapter;
- `prctl(PR_GET_SECCOMP, ...)`; and
- `prctl(PR_SET_SECCOMP, STRICT|FILTER, filter)`.

Raw strict mode requires zero flags and a NULL `uargs`. The historical prctl
strict entry ignores its optional filter argument by passing NULL to the
common operation. The prctl mode remains a full machine word, so nonzero high
bits are not truncated into a valid mode.

The adapter byte-copies the 16-byte 64-bit `sock_fprog` header, its complete
instruction array, and the four-byte action query. Userspace pointers may be
unaligned; typed Rust pointer alignment is not added to Linux's usercopy
contract. A NULL header is `EFAULT`; a zero/oversized length or a nonzero
length with a NULL instruction pointer is `EINVAL`; a non-NULL bad instruction
pointer is `EFAULT` after the preceding checks.

Filter installation follows this order:

1. reject unsupported flags;
2. copy the `sock_fprog` header;
3. validate its source length;
4. take one exact credential snapshot and require `no_new_privs` or
   `CAP_SYS_ADMIN` in the current user namespace;
5. copy and verify the instruction array;
6. prepare and charge an unpublished immutable leaf; and
7. revalidate the expected task-local leaf and publish it under the short
   publication lock.

An error before step 7 leaves the task state unchanged. A failed revalidation
does not publish the prepared node.

### 3. Separate source length, converted path charge, and live bytes

One userspace program contains between one and 4096 classic-BPF instructions.
That source length drives the per-program limit and logical program-byte
charge.

Linux v6.12 performs seccomp path admission after classic-BPF preparation and
uses the stored `bpf_prog::len`. Layer 2 therefore records a separate frozen
unblinded cBPF-to-eBPF migration charge:

- three instructions for the migration prologue;
- two for `RET_K` (`MOV32` plus `EXIT`);
- five for register division and its divide-by-zero guard;
- one or two for a conditional branch, depending on whether one target can
  fall through;
- one additional temporary-materialization instruction for the applicable
  negative-immediate comparisons; and
- one for every other accepted seccomp instruction.

Appending a filter charges its converted length plus four for every existing
ancestor. A complete path of exactly 32768 is accepted. A path greater than
32768 fails with `ENOMEM` without publishing a node.

This arithmetic is a Linux v6.12 unblinded converted-length compatibility
baseline. TheKernel does not materialize an eBPF translation or native JIT
image to enforce the filter.

The third quantity is TheKernel's architecture-independent aggregate live
budget. The adapter creates one boot-time 16 MiB budget shared by all tasks.
Each newly allocated immutable node reserves:

```text
source_instruction_count * sizeof(classic_bpf_instruction)
+ sizeof(private_filter_node)
```

Fork and clone share an existing node and do not charge it again. Failed
construction and failed publication preparation release their unpublished
owners. The final immutable owner refunds the charge. Budget exhaustion maps
to `ENOMEM`; it is not RLIMIT_MEMLOCK, a physical-memory promise, native JIT
memory, or the Linux `bpf_jit_limit` sysctl.

### 4. Publish one immutable task-local ancestry

Each `Thread` owns one authoritative `SeccompState` publication slot and one
atomic disabled fast-path bit. Disabled syscall entry reads the bit and takes
no publication lock. A filtered task takes one short lock only to clone the
complete immutable state; evaluation occurs after unlock and allocates
nothing.

Program verification, path computation, budget reservation, and node
allocation occur before the publication lock. Commit verifies that the live
leaf is the exact expected identity and that the prepared leaf directly
extends it. Equivalent bytecode on another branch is not equivalent ancestry.

Clone and fork take one caller snapshot and initialize an independent child
publication slot. Existing immutable nodes and their charge are shared. A
later child or thread append changes only that task's leaf; neither its parent
nor an already-created sibling changes without a future TSYNC transaction.
Exec preserves the calling task's state.

Exit explicitly swaps the authoritative state to Disabled only after task exit
is irreversible. The old ancestry is returned and dropped after process
lifecycle and graph locks have been released. Therefore an exited task object
retained by procfs, pidfd, a joiner, or scheduler GC does not retain the
task-local seccomp budget charge. Destruction iteratively unwraps unique
ancestors so a legal maximum-depth chain cannot recurse through thousands of
kernel-stack frames.

### 5. Enforce before syscall decoding and fast paths

Seccomp observes the raw syscall number, audit architecture, post-syscall
instruction pointer, and six raw arguments before generic `Sysno` decoding,
getter fast paths, time fast paths, restart admission, VFS work, or any other
syscall side effect.

Strict mode permits only `read`, `write`, `exit`, and `rt_sigreturn`. Any
other syscall terminates the calling task with uncatchable SIGKILL. Strict and
filter modes are irreversible and cannot replace each other.

Filter mode evaluates every immutable program newest to oldest. The action
with the lowest signed full-action value wins; a tie retains the newest
filter's data and metadata. Unknown action values fail closed as
KILL_PROCESS.

| Action | Current adapter behavior | Explicit boundary |
| --- | --- | --- |
| `ALLOW` | Execute the syscall. | None within this profile. |
| `ERRNO` | Skip execution; return zero for data zero or negative errno capped at 4095. | No userspace side effect occurs before the result. |
| `TRAP` | Restore the architecture syscall frame, queue forced synchronous SIGSYS, and populate `si_errno`, `si_syscall`, `si_arch`, and `si_call_addr`. | The ordinary signal subsystem owns handler/default delivery. |
| `KILL_THREAD` | Restore the frame and synchronously terminate only the calling task with SIGSYS. | Exact Linux last-live-thread core-dump behavior is not claimed. |
| `KILL_PROCESS` | Restore the frame, establish group exit, and terminate the complete thread group with SIGSYS status. | Exact core generation and core-note registers are not claimed. |
| unknown | Treat as `KILL_PROCESS`. | It never becomes ALLOW. |
| `LOG` | Emit at most 1024 boot-global decision records plus one suppression notice, then allow the syscall. | This is not Linux audit, audit policy, sysctl control, or loss accounting. |
| `TRACE` | With no tracer-owned seccomp event lifecycle, skip with `ENOSYS`. | Not reported available by `GET_ACTION_AVAIL`. |
| `USER_NOTIF` | With no listener-owned request lifecycle, skip with `ENOSYS`. | Not reported available and no notification sizes are exposed. |

KILL actions call the terminal exit path directly. They must not be converted
to an ordinary queued SIGSYS, because an unblocked user handler must never
catch a KILL action.

The table above freezes a deliberate TheKernel profile: TRAP and every KILL
branch restore the architecture syscall frame before their terminal action.
Linux v6.12 also rolls back TRAP and KILL branches that reach forced
SIGSYS/core handling, but its non-final `KILL_THREAD` shortcut calls
`do_exit(SIGSYS)` without rollback. TheKernel does not claim byte-for-byte core
or register-note parity for that shortcut; the unified rule is the documented
RV64/LoongArch64 consumer contract.

### 6. Reject incomplete external lifecycles

Every nonzero `SECCOMP_SET_MODE_FILTER` flag is currently rejected with
`EINVAL`, including:

- `SECCOMP_FILTER_FLAG_TSYNC` and `TSYNC_ESRCH`;
- `SECCOMP_FILTER_FLAG_NEW_LISTENER` and `WAIT_KILLABLE_RECV`;
- `SECCOMP_FILTER_FLAG_LOG`; and
- `SECCOMP_FILTER_FLAG_SPEC_ALLOW`.

Layer 2 contains non-mutating exact-ancestry checks useful to a future TSYNC
adapter. It does not freeze a thread group, propagate `no_new_privs`, allocate
all sibling state, publish atomically, or return a Linux failing TID. Layer 3
must not expose TSYNC until one process-wide transaction owns all of those
steps.

`SECCOMP_GET_NOTIF_SIZES` with zero flags returns `EOPNOTSUPP`; nonzero flags
return `EINVAL` first. There is no listener FD, request identity, response
table, addfd operation, readiness source, cancellation rule, or
single-completion owner.

TRACE and USER_NOTIF action classes remain representable so an untrusted
filter cannot turn them into ALLOW. Their no-owner `ENOSYS` result does not
advertise ptrace-event or listener support.

`SECCOMP_RET_LOG` is a bounded diagnostic convenience. Exact Linux audit
records, per-filter `FILTER_FLAG_LOG`, `seccomp_actions_logged`, audit
credentials, and administrator policy are unsupported.

KILL status and thread-group scope are supported, but Linux-compatible core
dump exactness is not. In particular, this profile does not promise
`WCOREDUMP`, a complete LoongArch general-register note, last-live-thread
KILL_THREAD core selection at the exit linearization point, or byte-for-byte
Linux core files.

### 7. Do not claim JIT or hardening parity

The filter interpreter is allocation-free and bounded by verified forward
control flow. Version 0.1 does not include:

- a classic-BPF or eBPF JIT;
- native executable-image byte accounting;
- `bpf_jit_limit` behavior;
- constant blinding or its configuration-dependent instruction expansion;
- exact path accounting under `bpf_jit_harden`;
- BTF, CO-RE, maps, helpers, bounded loops, or program links; or
- a socket-filter attachment contract.

The unblinded converted charge is intentionally stable across TheKernel's
supported consumers. A future JIT or hardening mode must define whether path
admission remains on this frozen value or adopts a new versioned execution
charge. It may not silently change the 32768 boundary.

### 8. Keep allocation and lock rules explicit

- Disabled syscall entry is one atomic read.
- Filtered entry clones one immutable state under the task-local
  `SpinNoIrq`; evaluation happens after unlock.
- Usercopy and program verification never occur under the publication lock.
- Filter preparation reserves its complete node charge before publication.
- Publication performs no userspace access and no program allocation.
- Evaluation allocates nothing, mutates no program, and executes no backward
  branch.
- Exit swaps the state under the short publication lock, then destroys the old
  ancestry after all process lifecycle/graph locks are gone.
- Aggregate charge uses checked atomic reservation and exact RAII refund.
- No benchmark, workload name, executable identity, or runtime profile changes
  these safety or resource rules.

## Non-goals for the initial profile

The 0.1 contract does not implement or advertise:

- group-wide TSYNC and its failing-TID/ESRCH result;
- user-notification listeners, request queues, responses, addfd, cancellation,
  or readiness;
- ptrace `PTRACE_EVENT_SECCOMP`, trace recheck, or syscall rewriting;
- Linux audit fidelity or filter-install logging flags;
- exact Linux fatal core-dump production;
- 32-bit compat tasks or mixed audit architectures;
- a JIT, constant blinding, native code-memory accounting, eBPF, BTF, maps,
  helpers, or socket-filter ownership; or
- a claim that recognizing a UAPI constant completes its lifecycle.

These are dependency-ordered extensions, not hidden configuration switches.

## Rejected alternatives

- Putting Linux seccomp policy in `thekernel-axcbpf`: this would make a generic
  mechanism depend on tasks, syscall layouts, actions, and signals.
- Reusing the existing eBPF subsystem as an unreviewed classic-BPF verifier:
  the accepted instruction profile and lifecycle are different.
- Verifying or allocating under the task publication lock: an untrusted filter
  could turn a short linearization point into an unbounded critical section.
- Mutating a shared filter vector after clone: it would violate task-local
  append isolation and make ancestry identity ambiguous.
- Charging only userspace source length to the 32768 path: Linux v6.12 checks
  the prepared program's converted length.
- Charging fork and clone for every inherited byte: immutable shared ancestry
  is one live allocation and should have one logical charge.
- Waiting for final `TaskInner` destruction to release filters: external task
  references can retain a dead task and cause false global-budget exhaustion.
- Exposing TSYNC by looping over siblings: allocation or membership changes can
  leave a partially synchronized thread group.
- Advertising TRACE, USER_NOTIF, LOG audit, or core exactness because an action
  enum exists: each requires an independently testable external lifecycle.
- Copying Linux GPL implementation code: the project reimplements contracts
  from public behavior and source review.

## Validation gates

This section records exact gate wiring and acceptance requirements. It is not
a statement that those commands passed at the current source revisions. A
result is evidence only when its log records the exact TheKernel,
`thekernel-ax`, and `thekernel-linux-abi` revisions used by that run.

### Layer 1 classic-BPF mechanism

The independent mechanism gate is:

```sh
cd ../thekernel-ax
cargo +1.85.0 test -p thekernel-axcbpf --all-targets --locked
cargo +1.85.0 test -p thekernel-axcbpf --doc --locked
cargo +1.85.0 clippy -p thekernel-axcbpf --all-targets --locked -- -D warnings
RUSTDOCFLAGS='-D warnings' \
  cargo +1.85.0 doc -p thekernel-axcbpf --no-deps --locked
cargo +1.85.0 check -p thekernel-axcbpf --locked \
  --target riscv64gc-unknown-none-elf
cargo +1.85.0 check -p thekernel-axcbpf --locked \
  --target loongarch64-unknown-none
scripts/package-unpack-original.sh thekernel-axcbpf
```

It must cover empty/oversized programs, every accepted and rejected opcode,
division and shift boundaries, scratch initialization on all paths, forward
jump targets, fallible allocation, immutable ownership, and allocation-free
evaluation.

### Layer 2 Linux policy core

The Linux ABI workspace gate is:

```sh
cd ../thekernel-linux-abi
CARGO_TOOLCHAIN=nightly-2025-05-20 ./scripts/ci.sh
```

For seccomp this includes workspace formatting, warnings-denied clippy,
all-feature tests, warnings-denied rustdoc, no-default-feature checks,
RISC-V/LoongArch no-std target checks, provenance, package/extract validation,
and the local pre-publication package closure against the exact axcbpf archive.

The focused commands wired into TheKernel's per-commit gate are:

```sh
cargo +nightly-2025-05-20 test --locked \
  --manifest-path ../thekernel-linux-abi/Cargo.toml \
  -p thekernel-linux-seccomp --all-features
cargo +nightly-2025-05-20 check --locked \
  --manifest-path ../thekernel-linux-abi/Cargo.toml \
  -p thekernel-linux-seccomp --no-default-features
```

Policy tests must cover the native 64-byte input layout, RV/LA audit values,
profile verifier, converted opcode weights, exact 32768 acceptance and
overflow rollback, signed action precedence and tie data, global budget
rollback/refund, cross-budget rejection, inherited ownership, stale
publication, TSYNC eligibility without mutation, and iterative maximum-depth
drop.

### TheKernel host and package-consumer gates

The canonical containerized host wiring is:

```sh
./scripts/ci/per-commit.sh
```

Relevant substeps are `axcbpf-core-tests`, `seccomp-core-tests`,
`seccomp-core-check`, `kernel-host-check`, `kernel-seccomp-adapter-tests`,
`ci-script-tests`, formatting, and vendor provenance. The adapter compile and
unit-test checks, with the canonical toolchain and sibling-repository
environment supplied by `per-commit.sh`, are equivalent to:

```sh
cargo check --locked --manifest-path kernel/Cargo.toml \
  --tests --features bpf --target x86_64-unknown-linux-gnu
cargo test --locked --manifest-path kernel/Cargo.toml \
  --tests --features bpf,axtask/test --target x86_64-unknown-linux-gnu \
  seccomp::tests -- --test-threads=1
```

The first command compiles the complete TheKernel host adapter and tests. The
second executes the data/frame helpers and exit-retirement ownership tests
without claiming a host syscall runtime. GitHub's per-commit job runs inside a
Docker job container and therefore inherits `SECCOMP_MODE_FILTER`. Its
CI-script check still compiles the portable
smoke binary, but records an explicit compile-only skip because inherited
policy invalidates mode-zero, strict-mode, filter-count, and path-limit
baselines. That skip is not adapter, guest, or Linux differential evidence.

The independent host Linux differential runs on the bare hosted runner:

```sh
./scripts/ci/seccomp-host-differential.sh
```

It refuses an inherited seccomp profile, runs the portable program only from
initial mode zero, and requires exactly:

```text
THEKERNEL_SECCOMP_KILL_SCOPE_OK
THEKERNEL_SECCOMP_RESOURCE_PORTABLE_OK
THEKERNEL_SECCOMP_OK
```

with no `THEKERNEL_SECCOMP_FAIL`. This proves the portable test program and
its host-side expectations, not TheKernel adapter runtime behavior. The
workflow keeps this as a separate, non-skippable runtime job so a container
compile-only skip cannot satisfy it. Whether repository branch protection
marks that job required is external policy and needs its own receipt.

The package-consumer gate is:

```sh
./scripts/ci/release-consumer-gate.sh \
  --arch both \
  --ax-head <exact-40-hex-head> \
  --linux-abi-head <exact-40-hex-head> \
  --output-release-set <receipt.tsv>
```

It packages and audits both seccomp crates, binds the extracted
`thekernel-linux-seccomp` lockfile to the exact axcbpf archive, rewrites a
temporary TheKernel consumer to those extracted artifacts, forbids source
workspace fallbacks, and performs locked offline `kernel-rv` and `kernel-la`
builds. It does not boot those packaged-consumer kernels. The source-workspace
RV/LA kernels booted below are therefore a separate evidence path, not runtime
validation of the packaged artifacts.

Before the first registry publication, Cargo cannot normalize the seccomp and
signal package locks by resolving axcbpf and usercopy from crates.io. The gate
therefore packages dependency roots first, vendors the locked registry graph,
and safely extracts both already-audited archives into a temporary Cargo
directory source with their exact archive checksums. It then assembles the
remaining Linux-ABI packages offline. The normalized archives must still
contain registry-only dependencies and crates.io lock entries whose checksums
match those coordinated archives. This models dependency order; the workspace
package command names crates.io explicitly as its eventual publication target,
but it is not evidence that any crate is already published.

### RISC-V and LoongArch guest gates

The direct semantic commands are:

```sh
./scripts/system-test.sh --arch rv
./scripts/system-test.sh --arch la
```

The orchestrated PR command is:

```sh
./scripts/ci/pr-gate.sh
```

It runs the package-consumer build gate, source-workspace dual-architecture
kernel/rootfs builds, boot-shell checks, and then both semantic system tests.
The current workflow executes this job only on a self-hosted QEMU runner when
`THEKERNEL_QEMU_CI=1` for a pull request or manual dispatch. A normal push to
`main` runs the per-commit and bare-host seccomp differential jobs, but not the
dual-architecture runtime gate.

Each architecture's guest log must contain these exact seccomp-tool lines:

```text
THEKERNEL_SECCOMP_API_OK
THEKERNEL_SECCOMP_FILTER_ERRORS_OK
THEKERNEL_SECCOMP_UNALIGNED_OK
THEKERNEL_SECCOMP_FILTER_OK
THEKERNEL_SECCOMP_ERRNO_OK
THEKERNEL_SECCOMP_FASTPATH_OK
THEKERNEL_SECCOMP_UNKNOWN_OK
THEKERNEL_SECCOMP_ERRNO_ZERO_OK
THEKERNEL_SECCOMP_LOG_OK
THEKERNEL_SECCOMP_TRAP_OK
THEKERNEL_SECCOMP_TRAP_ROLLBACK_OK
THEKERNEL_SECCOMP_INHERIT_OK
THEKERNEL_SECCOMP_THREAD_APPEND_ISOLATION_OK
THEKERNEL_SECCOMP_FORK_APPEND_ISOLATION_OK
THEKERNEL_SECCOMP_PROC_OK
THEKERNEL_SECCOMP_EXEC_OK
THEKERNEL_SECCOMP_STRICT_OK
THEKERNEL_SECCOMP_PRCTL_STRICT_OK
THEKERNEL_SECCOMP_STRICT_KILL_OK
THEKERNEL_SECCOMP_UNSUPPORTED_OK
THEKERNEL_SECCOMP_KILL_THREAD_OK
THEKERNEL_SECCOMP_KILL_PROCESS_OK
THEKERNEL_SECCOMP_KILL_UNKNOWN_OK
THEKERNEL_SECCOMP_KILL_SCOPE_OK
THEKERNEL_SECCOMP_EXIT_RECLAIM_OK
THEKERNEL_SECCOMP_RESOURCE_OK
THEKERNEL_SECCOMP_RESOURCE_ROLLBACK_OK
THEKERNEL_SECCOMP_OK
```

The enclosing init must then emit:

```text
THEKERNEL_SYSTEM_TEST_SECCOMP_OK
THEKERNEL_SYSTEM_TEST_PASS
```

The gate rejects `THEKERNEL_SECCOMP_FAIL`, the general system-test failure
marker, panic, BUG, and Oops text. The exit-reclaim case must keep external
task/proc owners alive while more than 16 MiB of sequential child filter
charges would otherwise accumulate; prompt scheduler GC alone is not evidence
of explicit retirement.

The ordinary pthread control worker is joined normally. The worker killed by
`KILL_THREAD` publishes its TID before the filtered syscall and is instead
observed through bounded `tgkill(tgid, tid, 0)` polling until `ESRCH`. A kernel
seccomp kill bypasses libc's private pthread-exit bookkeeping; requiring
`pthread_join` for that worker would test a glibc implementation detail and is
[known to hang on musl](https://www.openwall.com/lists/musl/2019/06/26/5).
The TID-disappearance check directly proves the Linux kernel-visible scope
while the spawner's exit code proves that its sibling survived.

### Required additional evidence

Before this RFC can become `implemented`, one source-exact evidence set must
bind and retain:

- the three exact repository revisions and package checksums;
- complete Layer 1 and Layer 2 test/lint/doc/package logs;
- host adapter compilation and test-program logs;
- packaged-consumer RV/LA build outputs;
- source-exact RV and LA guest logs containing every marker above;
- negative logs proving unknown/unsupported advertisement remains closed; and
- deterministic resource rollback and exit-retirement evidence.

Until an exact packaged-consumer kernel is also booted, release evidence must
continue to distinguish packaged offline builds from source-workspace guest
execution; neither path may be presented as proving the other.

A later claim of Linux-exact KILL core behavior additionally requires
dual-architecture `WCOREDUMP` and core-note register validation. A later TSYNC,
TRACE owner, USER_NOTIF, audit, or JIT claim requires its own lifecycle,
failure, concurrency, and teardown gates.

## Status and acceptance gate

The source checkpoint contains the three-layer implementation and the gate
wiring described above. This RFC deliberately records no pass count and no
claim that the current worktree has completed host, packaged-consumer, RV, or
LoongArch execution. In-repository scripts are test definitions, not receipts.

The RFC remains `draft` until an exact-revision evidence set runs the complete
Layer 1, Layer 2, host, package-consumer, RV guest, and LoongArch guest gates.
It may move to `implemented` only if the adapter advertises no feature beyond
those receipts and the explicit unsupported boundaries remain fail-closed.

TSYNC, listener/notification ownership, ptrace seccomp events, Linux audit
fidelity, core-dump exactness, and JIT hardening are outside this initial
acceptance gate and require separately reviewed extensions.
