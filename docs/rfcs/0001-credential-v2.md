# RFC 0001: Immutable Credentials, User-ID Mapping, and Typed Security Hooks

- Status: draft
- Date: 2026-07-11
- Owners: TheKernel maintainers
- Target layer: common Linux ABI support, integrated in the kernel before
  extraction as `thekernel-linux-cred`

## Problem

The current process security identity is split across independent stores:

- real, effective, saved, and filesystem UID/GID values;
- effective, permitted, inheritable, bounding, and ambient capabilities plus
  securebits;
- supplementary groups;
- `no_new_privs`;
- a lightweight user-namespace pointer.

A reader can therefore observe fields from different updates. A set-ID
operation can publish IDs before its capability fixup, and a DAC snapshot can
combine an old group set with new filesystem IDs. The current namespace model
also stores userspace numeric IDs directly, has no `uid_map` or `gid_map`, and
approximates namespace capability direction without Linux's owner rule.

Exec does not yet implement one transactional set-ID/file-capability
transition. Dumpability, parent-death-signal clearing, ptrace safety, namespace
ownership, and security-module decisions are consequently incomplete.

This is a semantic and security defect, not merely a crate-layout problem.

## Evidence reviewed

The design was checked against these exact source snapshots on 2026-07-11:

- Linux master `dd3210c47e8d3ac6b4e9141fc68acc03b38c0ba3`:
  - `include/linux/cred.h` and `kernel/cred.c` for immutable committed
    credentials, `prepare_creds()`, `commit_creds()`, objective/subjective
    credentials, RCU publication, dumpability ordering, and deferred release;
  - `include/linux/uidgid.h`, `include/linux/user_namespace.h`, and
    `kernel/user_namespace.c` for `kuid_t`/`kgid_t`, bounded non-overlapping
    extent maps, one-time publication, `setgroups` policy, and namespace
    ownership;
  - `security/commoncap.c` for `ns_capable()` ancestry/owner rules, set-ID
    fixups, file capability revisions, ambient/bounding rules, and exec
    transitions;
  - `security/security.c`, `include/linux/lsm_hooks.h`, and
    `include/linux/lsm_hook_defs.h` for ordered stacked hooks and prepare,
    check, commit, and free phases.
- FreeBSD main `86691d52a6d3796ad36ba474cf0a9493f6d99202`:
  - `sys/sys/ucred.h` and `sys/kern/kern_prot.c` for credentials whose identity
    fields are constant after publication, copy-on-write updates, complete
    construction before MAC checks, and a single process credential swap;
  - `sys/security/mac/mac_framework.c` for ordered static/dynamic policy
    dispatch and explicit synchronization.

Linux source is GPL-2.0-only and FreeBSD source is BSD-licensed. This RFC adopts
observable semantics and architecture ideas; it does not copy implementation
code.

## Decision

### 1. Kernel ID types

Introduce distinct newtypes:

```rust
pub struct Kuid(u32);
pub struct Kgid(u32);
pub struct UserUid(u32);
pub struct UserGid(u32);
```

`Kuid` and `Kgid` are IDs in the kernel-global identity space. VFS inode
ownership, IPC ownership, accounting keys, signal-pending ownership, and
credential internals use these types. Syscall values use `UserUid` and
`UserGid` until they are explicitly mapped through a user namespace.

The all-ones value is invalid internally. Mapping an input ID that has no
extent fails with `EINVAL`. ABI output uses the configured overflow UID/GID
only for Linux interfaces whose contract calls for a munged value; interfaces
that report mapping failure must not silently substitute zero.

Raw `u32` conversions remain private to the ABI and persistence adapters.

### 2. Immutable credential object

One committed credential contains all security identity fields that must be
sampled together:

```rust
pub struct Cred {
    ids: CredIds<Kuid, Kgid>,
    groups: Arc<GroupInfo>,
    caps: CapabilitySets,
    securebits: SecureBits,
    no_new_privs: bool,
    user_ns: Arc<UserNamespace>,
    security: SecurityCredContext,
}
```

`GroupInfo` is sorted and deduplicated before publication. It is bounded by
`NGROUPS_MAX`; construction is fallible and happens outside spin locks.
Committed `Cred` fields are never mutated. Capability invariants are checked
when a builder is finalized, including:

- effective is a subset of permitted;
- ambient is a subset of both permitted and inheritable;
- every set is masked to `CAP_LAST_CAP`;
- locked securebits cannot change;
- `no_new_privs` can only transition from false to true.

The initial root credential is constructed explicitly rather than relying on
field defaults that accidentally grant future capabilities.

### 3. Prepare, authorize, and commit

Every credential-changing operation follows four phases:

1. copy user input and resolve mapped IDs;
2. take one immutable old-credential snapshot and build `PreparedCred`
   fallibly without publishing it;
3. run capability fixups and ordered typed security hooks against old and
   proposed credentials;
4. atomically publish the new `Arc<Cred>` and then release the old ownership
   outside the publication lock.

The first implementation uses a sleepable credential-update mutex plus a
short `SpinNoIrq<Arc<Cred>>` publication slot. Readers clone one `Arc` while
holding the short lock. Writers serialize prepare/authorize/commit so two
threads cannot lose one another's changes. No allocator, hook, destructor, or
waker operation runs under the publication spin lock.

This deliberately provides Linux-style atomic read semantics without claiming
Linux's RCU implementation. A later profile may replace the read-side lock
with RCU or epoch reclamation only if credential reads are measurably hot and
the full deferred-destruction contract is proven on both architectures.

The API shape is:

```rust
fn current_cred(&self) -> Arc<Cred>;
fn prepare_cred(&self, operation: CredOperation) -> Result<PreparedCred, CredError>;
fn commit_cred(&self, prepared: PreparedCred) -> Result<CredCommitEffects, CredError>;
```

`PreparedCred` is bound to the update transaction and cannot be reused after
abort or commit. Dropping it aborts without observable changes.

TheKernel does not initially expose arbitrary subjective-credential override.
The internal slot keeps room for separate objective and subjective references,
but they remain identical until an audited kernel service requires a scoped
override guard.

### 4. Commit side effects and ordering

Credential publication and related process state form one ordered operation.
The prepared result records whether the transition:

- changes effective or filesystem IDs;
- reduces the caller's authority;
- gains authority through set-ID or file capabilities;
- changes the real-user accounting key;
- requires procfs ownership/notification updates;
- clears the parent-death signal;
- changes dumpability.

When a transition requires a less-dumpable state, dumpability is published
before the new credential becomes visible to ptrace/procfs readers. Old
credentials and old security contexts are dropped only after publication locks
are released. Failure before publication leaves credentials, dumpability,
accounting, and notifications unchanged.

Dumpability belongs to executable address-space/process state, not inside
`Cred`. The credential module returns an explicit commit effect rather than
reaching into MM through a hidden global.

### 5. User namespaces and ID maps

`UserNamespace` owns:

- stable namespace identity, parent, depth, owning `Kuid`/`Kgid`, and creator
  `CAP_SETFCAP` fact;
- immutable published UID and GID maps;
- a write-once `setgroups` policy;
- bounded per-user resource-accounting tables.

An ID map is a sorted immutable array of at most 340 extents. Construction
rejects zero length, overflow, duplicate/overlapping upper ranges,
duplicate/overlapping lower ranges, unmappable parent ranges, and invalid IDs.
Forward and reverse indexes are prepared before one pointer publication. The
simple implementation may use two sorted boxed slices and binary search; it
does not copy Linux's inline-five optimization until measurement justifies it.

Map writes are accepted once, from offset zero, through the procfs adapter.
Permission checks preserve Linux's single-ID unprivileged mapping rule,
`CAP_SETUID`/`CAP_SETGID`, `CAP_SETFCAP` protection for parent UID 0, and the
requirement to deny `setgroups` before an unprivileged GID map is installed.

Creating or joining a user namespace produces a proposed credential and uses
the same prepare/commit transaction. Other namespace objects gain an explicit
owning user namespace; they no longer infer authority from a process-global
namespace pointer.

ID-mapped mounts are a later VFS slice. Credential v2 establishes types and
conversion rules without pretending ordinary user-namespace maps already make
a mount idmapped.

### 6. Namespace capability checks

All namespace-sensitive privilege checks use one operation:

```rust
fn ns_capable(cred: &Cred, target: &UserNamespace, cap: Capability) -> bool;
```

The rule matches Linux's direction:

- in the credential's own namespace, the requested effective capability must
  be set;
- a credential has authority over descendants when the ancestry walk reaches
  its namespace with the capability set;
- the owner of an immediate child namespace has all capabilities in that child
  and its descendants;
- no capability applies upward to an ancestor or sideways to a sibling.

Call sites pass the namespace that owns the object being changed. Bare
`euid == 0`, `uid == 0`, and process-relative `has_capability()` checks are
removed from namespace-sensitive paths.

### 7. Complete set-ID and exec transitions

Setuid/setgid/setreuid/setresuid/setfsuid and group/capability changes operate
on one builder. UID fixups, filesystem capability masking, ambient
reconciliation, and securebits are applied before publication, never through a
second capability lock.

Exec prepares credentials before replacing the address space and commits them
only at the irreversible exec boundary. The transition includes:

- setuid/setgid inode bits, mount `nosuid`, tracing, and `no_new_privs`;
- `security.capability` revisions 1, 2, and namespaced revision 3 with strict
  size, endianness, root-ID, and capability-mask validation;
- inheritable, permitted, bounding, ambient, effective, and securebits rules;
- saved and filesystem ID updates;
- privilege-gain detection, secure-exec state, dumpability, parent-death
  signal, and procfs-visible effects;
- typed `bprm`-style prepare/check/commit hooks.

Interpreter resolution continues to use the frozen pre-exec filesystem
credential. A failed interpreter load or executable mapping aborts the proposed
credential. No set-ID or file capability becomes visible on failed exec.

### 8. Typed stacked security hooks

TheKernel adopts Linux LSM's hook topology, not its untyped `void *` blobs or
macro-generated C call surface.

Hooks are grouped by typed operation context, initially covering:

- credential allocation, cloning, set-ID fixup, capability changes, and free;
- exec credential derivation, executable checks, and commit;
- ptrace, process memory, pidfd, signal, and scheduler authority;
- inode permission/create/link/unlink/rename/setattr/xattr and file open;
- socket create/bind/connect/send/receive and Unix peer lookup;
- mmap/mprotect and future fault delegation.

Each context contains explicit actor credentials, target ownership namespace,
object identity, requested operation, and already-resolved DAC facts. Hooks do
not call `current()` or repeat pathname lookup.

The boot-time registry is fallibly built and frozen before userspace starts.
Dispatch order is stable. Authorization hooks are deny-first and stop on the
first error; notification hooks run after successful commit. Hook functions do
not allocate unless their contract explicitly admits allocation before the
operation's publication point. Per-module credential state is constructed as
part of `PreparedCred` and destroyed outside spin locks.

The initial built-in modules are:

- capability semantics;
- a no-op policy module used to test stacking and ordering;
- optional audit/deny test modules under test or failpoint builds.

An empty registry is not advertised as a complete LSM policy. The typed hook
framework and its call-site coverage can be implemented before a mandatory
access-control policy is selected.

## Locking and allocation contract

- Usercopy and ID-map parsing happen before the credential-update mutex.
- Credential builders may allocate only in sleepable task context.
- The publication spin lock protects pointer replacement only.
- `Arc`/group/security-context destruction occurs after every spin lock is
  released.
- Namespace map publication is write-once; readers see either the old empty map
  or the complete new indexes.
- LSM hook order is fixed after boot and cannot race module removal.
- No hook may retain borrowed syscall or pathname data.
- Resource accounting admission precedes credential publication and has an
  explicit rollback token.

## Error contract

- Invalid IDs, maps, capability bits, securebits, or xattrs: `EINVAL`.
- Missing authority for a transition or map write: `EPERM`.
- An ID not mapped in the caller's namespace: `EINVAL`, unless a specific read
  ABI requires overflow-ID output.
- Allocation failure before publication: `ENOMEM` with no state change.
- Accounting limit: the Linux-defined limit error for that operation, not a
  fabricated `ENOMEM`.
- A concurrent transition is serialized internally; userspace does not see a
  private retry error.

## Validation gates

### Semantic tests

- Atomic snapshots never mix IDs, groups, capabilities, securebits,
  `no_new_privs`, or user namespace across concurrent updates.
- Every set-ID syscall matrix is tested with and without relevant capability,
  saved IDs, securebits, and mapped IDs.
- UID/GID maps cover boundary values, overflow, overlap, reverse lookup,
  one-write rules, setgroups policy, namespace ownership, and unmapped output.
- `ns_capable()` covers same, ancestor, child-owner, grandchild, sibling, and
  unrelated namespace cases.
- Exec tests cover set-ID, nosuid, no_new_privs, ptrace downgrade, file caps
  v1/v2/v3, bounding/ambient rules, interpreter failure, and secure exec.
- Ptrace, procfs, `process_vm_*`, pidfd, signal, mount, namespace, and DAC paths
  consume one frozen credential and the target object's owner namespace.
- Hook stacking tests cover order, denial, prepare rollback, commit
  notification, and context lifetime.

### Fault and concurrency tests

- Every allocation point in group/map/security-context/exec preparation is
  failpoint tested and leaves the old credential and accounting unchanged.
- Readers race repeated UID/group/capability transitions without observing an
  invalid invariant.
- Exec failure races ptrace/procfs reads without exposing gained authority or a
  dumpable privileged image.
- Namespace teardown waits for credential/object references and rejects new
  accounting admission.
- Destructors are instrumented to prove none run under publication spin locks.

### System tests

- Focused LTP credential, capability, user-namespace, ptrace, procfs, signal,
  and exec cases on RISC-V and LoongArch.
- Multi-process namespace/map stress and fork/exec loops under memory pressure.
- Both architectures boot with capability enforcement enabled; disabling
  hooks must be an explicit build policy, never an error fallback.

### Performance tests

Measure credential snapshot latency, pathwalk/open throughput, signal/ptrace
checks, and exec/setuid rates with counters disabled. Compare the lock-protected
`Arc` baseline with any proposed RCU/epoch version. RCU is accepted only if it
has a material repeatable gain and its grace-period, teardown, and memory
pressure behavior pass the same gates.

## Migration sequence

1. Add typed IDs, immutable `Cred`, group/capability invariants, and atomic
   snapshot/update APIs while retaining existing syscall behavior.
2. Convert all credential writers to prepare/commit and delete split stores.
3. Convert centralized DAC/process-access consumers to `Arc<Cred>` snapshots.
4. Add UID/GID maps, setgroups policy, object namespace ownership, and exact
   `ns_capable()`.
5. Complete exec file-capability/set-ID/dumpability transitions.
6. Introduce and cover typed stacked hooks.
7. Convert VFS/IPC persistence to kernel IDs and map at ABI boundaries.
8. After multiple stable vertical slices, extract the module as
   `thekernel-linux-cred` in the 0.x Linux-ABI workspace.

No step keeps compatibility accessors that resample independent mutable
stores. Temporary adapters must take one `Arc<Cred>` and be deleted as their
consumers migrate.

## Alternatives rejected

- **Keep separate atomics/locks:** cannot provide a coherent Linux
  `current_cred()` snapshot or transactional exec transition.
- **Move files into a crate first:** freezes the wrong globals and split-state
  API before semantics are stable.
- **Use RCU immediately:** adds unsafe reclamation and teardown complexity
  without current profiling evidence.
- **Treat namespace UID 0 as global root:** breaks kernel-ID ownership and
  grants authority across unrelated namespaces.
- **One untyped generic LSM callback:** hides operation data, encourages
  `current()` lookups, and makes hook coverage and lifetime review weaker.
- **Copy Linux's map storage layout:** the observable map rules matter; its
  inline-five/cache-line optimization must earn its place through measurement.

## Completion condition

This RFC becomes `implemented` only when the split credential stores are gone,
all named semantic/fault/concurrency/system gates pass on both architectures,
and no relevant permission path falls back to raw UID 0 or a process-global
capability check. Crate extraction and API freeze are separate later gates.
