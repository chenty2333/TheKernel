# thekernel-linux-vfs

`thekernel-linux-vfs` is the Linux-visible policy layer between a generic VFS
walker and thin syscall entry code. It provides:

- an immutable, explicit `PathContext` with credential, namespace, root, cwd,
  security-hook, resolve-policy, and resource-limit snapshots;
- strict `openat2()` resolve-flag validation and typed topology decisions;
- bounded component, symlink, restart, mount-crossing, and retry accounting;
- Linux `generic_permission()`-style DAC and capability fallback;
- protected-hardlink source safety with explicit own-user-namespace mapping
  and capability facts;
- sticky-directory and SGID/umask create policy;
- move-only, exact-snapshot `chmod`/`chown` plans that preserve UID/GID
  omission across the inode-hook boundary and produce backend-ready committed
  facts; and
- an RAII mutation transaction that revalidates, runs final policy admission,
  publishes at most once, and rolls every prepared failure back.

The crate is `no_std`, stable Rust 1.85 compatible, and owns no filesystem
tree. It does not call `current()`, read a global cwd/root, translate errno, or
perform a second textual-prefix permission walk. A consumer connects the
types to the topology events and stable handles produced by its generic VFS.

## Boundary

The generic VFS remains responsible for actual component lookup, symlink and
mount traversal, cache revalidation, object lifetime, filesystem I/O, and
atomic publication primitives. This crate decides Linux-visible admission and
scoping over those mechanisms. Syscalls should only decode user ABI values,
resolve `dirfd` to a retained start handle, construct one context, invoke the
adapter, and map typed errors to errno.

POSIX ACL evaluation, idmapped-mount translation, LSM hook implementations,
and read-only/noexec mount state remain explicit adjacent stages. Version
0.1.0 exposes the seams needed to add them without making a hidden global or a
particular RCU/cache algorithm part of the contract.

The setattr plans do not dispatch hooks or remove privilege xattrs. A kernel
adapter derives its typed hook proposal from the plan, runs the hook, consumes
the same plan for owner/capability/SGID authorization, and performs killpriv
and publication through filesystem-specific mechanisms.

## Error mapping

The kernel adapter normally maps:

- `WalkError::CrossDevice` to `EXDEV`;
- `WalkError::SymbolicLinkLoop` to `ELOOP`;
- `WalkError::RetryWithoutCached` to `EAGAIN`;
- path byte/component limits to the corresponding pathname error;
- `DacError::AccessDenied` to `EACCES`; and
- `DacError::StickyDenied` to `EPERM`; and
- every `SetattrError` variant to `EPERM`.

Unknown or incompatible `openat2()` resolve flags are rejected during context
construction; they are never accepted and ignored.
