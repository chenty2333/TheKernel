# RFC 0002: Explicit Linux Path Context over a Policy-Neutral VFS Walker

- Status: draft
- Date: 2026-07-12
- Owners: TheKernel maintainers
- Target layers: generic VFS mechanism and `thekernel-linux-vfs`

## Problem

Pathname lookup is a security operation, not string concatenation. A correct
lookup must apply directory-search permission to the directories actually
traversed after symlink expansion, `..`, absolute restarts, and mount
crossings. It must also retain one operation context across lookup, final
component checks, and create/open publication.

TheKernel now has a generic `PathwalkPolicy`/admission seam and a Linux DAC
adapter, but their public ownership boundary is not yet frozen. The remaining
risk is to publish a crate API that either reads the current task implicitly or
cannot later express scoped lookup, mount ownership, idmapped mounts, LSM
hooks, atomic create, and safe cache revalidation.

## Evidence reviewed

The design was checked against these exact source snapshots on 2026-07-12:

- Linux `44696aa3a489d2baf58efa61b37833f100072bee`:
  - `fs/namei.c` for `path_init()`, `link_path_walk()`, `may_lookup()`,
    `walk_component()`, symlink restart, mount traversal, and RCU-to-refwalk
    fallback;
  - `include/linux/namei.h` and `include/uapi/linux/openat2.h` for bounded
    symlink traversal and `RESOLVE_BENEATH`, `RESOLVE_IN_ROOT`,
    `RESOLVE_NO_XDEV`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`, and
    `RESOLVE_CACHED` contracts;
  - `inode_permission()` and the LSM permission hook ordering for per-directory
    traversal and final-object authorization.
- FreeBSD `62e22d7cfc1ca1c25bede6aaeca370c163a9a1ef`:
  - `sys/kern/vfs_lookup.c` and `sys/sys/namei.h` for explicit `nameidata`,
    credential-bearing component lookup, Capsicum-relative lookup,
    `RBENEATH`, mount-crossing control, MAC checks, and restartable cache
    lookup.
- TheKernel commits `ba0a4aa6536af5a092f74da9e79f097613a2bcca`,
  `a4cc43b`, `a135a77`, `f66f1ff`, and `ff08a23` for the current per-component
  admission hook, topology-edge policy, generation-stable runtime inode data,
  transactional node publication, mount lifecycle, and fallible path
  snapshots.

Linux source is GPL-2.0-only and FreeBSD source is BSD-licensed. This RFC
adopts observable contracts and architecture ideas; it does not copy source.

## Decision

### 1. Split mechanism from Linux policy

The generic ax/VFS layer owns:

- component iteration and bounded symlink expansion;
- `.` and `..`, root confinement, absolute restart, and mount traversal;
- cache lookup, revalidation, generation checks, and filesystem callbacks;
- an explicit policy callback at every topology-sensitive edge;
- fallible, transactional lookup/create primitives;
- mount and location ownership independent of Linux credentials.

`thekernel-linux-vfs` owns:

- Linux lookup/open/create/remove/rename intent;
- immutable filesystem-credential and target-user-namespace snapshots;
- directory-search, final-object DAC, sticky-bit, capability, and typed LSM
  authorization;
- Linux `*at`, `AT_*`, `openat2()` resolve, noexec, read-only-mount, and errno
  rules;
- Linux-visible ownership mapping and future idmapped-mount policy.

Syscalls only copy and decode arguments, select an FD/root/cwd snapshot, invoke
the Linux VFS operation, and copy results out.

### 2. One explicit operation context

Every Linux path operation receives an immutable context conceptually shaped
as:

```rust
pub struct PathContext<C, N, H> {
    pub credentials: C,
    pub mount_namespace: N,
    pub root: LocationHandle,
    pub cwd: LocationHandle,
    pub resolve: ResolvePolicy,
    pub security_hooks: H,
}
```

The concrete public API may use private fields and constructors, but it must
not call `current()`, read a global cwd/root, or resample credentials during an
operation. `dirfd` resolution occurs before the walk and contributes a stable
starting location plus rights snapshot.

The context records the user namespace owning each relevant namespace or
mount object. A later idmapped-mount implementation can add a mount ID mapping
without changing kernel IDs back to untyped integers.

### 3. Typed topology events

The generic walker reports typed events rather than Linux flags:

- search a directory before component lookup;
- follow a symlink, including whether it is final;
- restart from an absolute target;
- traverse `..` toward or at the operation root;
- cross into or out of a mount;
- use a cached result or require blocking revalidation;
- authorize the actual parent before create/remove/rename publication.

A policy may deny an event without performing a second textual-prefix walk.
No policy callback runs while a VFS spin lock is held. The callback receives
stable object handles and operation metadata, never borrowed syscall memory.

### 4. Scoped lookup and error contract

The Linux adapter preserves the observable `openat2()` rules:

- beneath/in-root escape attempts fail without transiently exposing an escaped
  object;
- no-xdev applies to bind mounts as well as ordinary mount points;
- no-symlink and no-magiclink are distinguished even if TheKernel initially
  has no magic-link provider;
- cached-only lookup returns retry/unsupported honestly rather than silently
  performing I/O;
- symlink loops and the global follow budget report `ELOOP`;
- denied traversal reports `EACCES`; malformed flags report `EINVAL`; scoped
  topology violations preserve their Linux error class.

Unsupported independent resolve flags may return an explicit unsupported
error in 0.x. They must not be accepted and ignored.

### 5. Transactional mutation

Open-with-create, mkdir, mknod, link, unlink, and rename use a prepared
operation:

1. copy and validate user input;
2. complete the real parent walk under one context and one symlink budget;
3. authorize search plus mutation intent on the actual parent and target;
4. reserve all fallible metadata and accounting;
5. revalidate the parent/name generation;
6. publish once through the filesystem operation;
7. roll back reservations on every failure.

A lookup-followed-by-create sequence is not treated as atomic unless the
filesystem primitive makes it so. No syscall-local prefix check substitutes
for this contract.

### 6. Ownership, locking, and cache rules

- `LocationHandle` keeps its mount and inode alive, including after lazy
  detach, but does not keep a parent/child mount cycle alive.
- Normal unmount prevents new admissions, flushes, revalidates external
  references, and rolls back on failure; lazy detach separates reachability
  from lifetime.
- Cache entries carry stable object identity/generation. Dynamic inode runtime
  data is prepared before publication and is not replaced by a competing
  lookup.
- Filesystem lookup may sleep or perform device I/O and therefore runs without
  directory cache spin locks.
- RCU/epoch pathwalk is an internal future optimization. The 0.1 contract
  exposes retry/revalidation semantics, not a particular reclamation method.

### 7. Resource bounds

The operation explicitly bounds:

- path and component byte lengths;
- symlink follows and nested restart state;
- mount/topology traversal;
- temporary component and audit storage;
- retry after cache invalidation or concurrent rename.

All user-triggered growth is fallible. Diagnostics are bounded and default
off on the hot path.

## Rejected alternatives

- Rechecking textual prefixes in each syscall: misses symlink targets, `..`,
  mount crossings, and filesystem-generated topology.
- Passing a closure that captures the current task implicitly: prevents stable
  snapshots, testing, and crate extraction.
- Freezing Linux `openat2` bits into the generic ax walker: leaks one ABI into a
  reusable mechanism.
- Adopting Linux RCU/refwalk or a lock-free pathname cache in 0.1: observable
  retry and lifetime rules matter now; the algorithm needs profiling first.
- Making every VFS object capability-only: useful seL4/Capsicum lessons inform
  handle ownership and rights, but replacing Linux pathname semantics would
  break the target ABI.

## Validation gates

### Semantic

- intermediate and final symlinks, absolute targets, `..`, trailing slash,
  empty path, no-follow, and dangling-create cases;
- beneath/in-root/no-xdev/no-symlink/cached-only matrices;
- search denial on the directory actually traversed;
- mount crossing, bind-style alias, lazy detach, namespace root, and stacked
  mount cases;
- create/remove/rename DAC, sticky bit, read-only, noexec, and rollback;
- immutable credential snapshot retained across concurrent credential change.

### Fault and concurrency

- allocation failure at every prepare point leaves no node, cache entry, mount
  record, or accounting residue;
- rename/unlink/mount/unmount racing lookup never returns a freed or wrong
  generation object;
- failed flush restores normal-unmount reachability;
- no allocator, destructor, hook, or filesystem I/O runs under an IRQ-safe
  spin lock.

### Consumer and performance

- host semantic tests plus RISC-V and LoongArch TheKernel builds/boots;
- fsx/fsstress and selected xfstests path/open/rename cases;
- pathwalk/open microbenchmarks with counters disabled;
- only after profiling may cache fast paths, RCU, epoch reclamation, or
  per-CPU lookup state replace the simple locked implementation.

## 0.1 extraction gate

`thekernel-linux-vfs` 0.1.0 may be published when the context, typed errors,
scoped-walk events, mutation preparation, and adapter traits compile without
TheKernel globals and pass the gates above. Internal cache/tree algorithms are
not frozen. The generic walker is released from `thekernel-ax` only after the
same Linux adapter consumes its packaged artifact on both architectures.
