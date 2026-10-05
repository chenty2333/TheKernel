# Container bring-up

Scope: x86_64, Linux 7.2.3 behavior; disposable QEMU/KVM guests only. No host
containers, host namespace changes, physical-disk writes, or online image pulls.
B1 tool-level closeout is complete; remaining field differences stay in
`procfs-sysfs-coverage.md` rather than extending this work.

## Known namespace gaps

### Mount namespace `setns` admission

The namespace's owner must grant `CAP_SYS_ADMIN`; the installed credential's
own user namespace must also grant `CAP_SYS_ADMIN` and `CAP_SYS_CHROOT`.
Authority failure is `EPERM`, before checking shared `fs_struct` ownership.
A direct namespace descriptor or mount-only pidfd request rejects a shared
`fs_struct` with `EINVAL`. A mixed pidfd namespace request instead prepares a
private temporary fs context, matching Linux `prepare_nsset`; it must not be
rejected merely because the original context was shared. User namespace entry
continues to enforce its separate single-thread and private-fs conditions.

TheKernel now makes these checks before resolving/preparing the target root,
using the existing task-user count, not transient `Arc` references. Existing
namespace/fs publication remains transactional. No ptrace access policy changed.

Regression: a pure host admission matrix and the paired task-control program
exercise ordinary entry, capability loss, `CLONE_FS` mount-only rejection,
and mixed mount+UTS pidfd entry. Host kernel tests: 2622 passed. q35 lint passed with 784 existing warnings.
KVM system guest: 70/70, no skips, normal shutdown (`system-1s33l4ej`).
Paired Linux 7.2.3/TheKernel task-control ABI: 14/14 selected contracts passed
(`abi-f7r40f04`); twelve isolated mount-entry children passed on both guests.
The differential probe does not establish racing clone/fs-sharing admission.

### Other known gaps (next)

- All originally listed namespace/clone construction gaps are now addressed.
- Source review found an additional direct non-mount setns installed-user
  admin check and PID ancestry errno discrepancy; fix next before tool entry.

### `clone3(CLONE_NNP)`

The complete unpublished child credential now includes `no_new_privs` before
one fork/user-namespace security admission and publication. Ordinary fork may
change only this monotonic restriction; every other credential field and
namespace/group identity must match its source. A new-user-namespace child can
also request NNP. Parent credentials remain unchanged; thread clones still
reject NNP with `EINVAL` under the existing Linux flag admission rules.

Validated: kernel host 2623 passed (including typed security publication,
inheritance, and NEWUSER+NNP); q35 lint passed with 784 existing warnings;
KVM system guest 70/70 (`system-k93x8ebc`); paired task-control 14/14 selected
contracts (`abi-8gos6_qh`). Actual children observe NNP=1, cannot clear it, and
leave the parent's NNP unchanged, both with and without NEWUSER.

### `clone3(CLONE_PIDFD_AUTOKILL)`

The clone pidfd is armed after complete task/cgroup/IPC publication and before
its fd becomes visible. Failed preparation/usercopy never arms it. The existing
final-OFD close hook (not backend `Arc` destruction or per-fd close) atomically
consumes the action and queues a kernel-originated process SIGKILL through its
weak `ProcessData` identity. It never looks up a reused numeric PID or rechecks
the closer's capabilities. Plain `pidfd_open` does not arm this action.
Existing admission requires PIDFD+AUTOREAP, disallows THREAD, and requires
CAP_SYS_ADMIN unless the request includes NNP. Clone pidfd status also now
retains Linux O_RDWR, thread O_EXCL and autokill O_TRUNC.

Validated: kernel host 2625, q35 lint (784 existing warnings), system guest
70/70 (`system-uyqbyc_e`), paired task-control 14/14 (`abi-9lvoilyu`). A real
blocked child survives closing one dup, then exits when the final clone-pidfd
OFD closes despite an independent non-autokill watcher remaining open; it is
actually autoreaped. CLOEXEC/status, invalid shapes and publication EFAULT
also pass on both guests. An initially missing host-test import was repaired
before this final validation. Concurrent SCM_RIGHTS/exec combinations are not
claimed tested. The original clone3 implementation gaps are resolved; its
contract is implemented. The registered raw contract assertions have passed;
this is not an assertion that every concurrency cross-product was executed.
The final static gate also requires a recorded errno-order review, not merely
an implemented status: the size/copy/scalar/set_tid/flag and AUTOKILL capability
ordering was checked against Linux `kernel/fork.c:2893-3052,2062-2094`. An
incomplete declaration initially blocked the next ABI run before either guest
ran; it was corrected without widening the shrink-only allowlist. Untested
SCM_RIGHTS/exec combinations remain explicit above, not claimed passes.

### `open_tree` / `fsmount` NAMESPACE and nsfs publication

An owned detached tree is adopted into a new graph above a private clone of
current's immutable namespace underlay. Construction never switches current's
namespace or mutates its graph. The existing source-FD-pinned ledger governs
recursive children and idmaps. Complete records/provider receipts are prepared
before attachment, with rollback before placement locks; the namespace owns
FUSE/NFS registration IDs after successful transfer. The existing private nsfs
provider publishes the real retained object, supporting statfs/type ioctl/setns.

Less-privileged owners receive one-way slave peers and placement locks. Captured
RO/nosuid/nodev/noexec and atime restrictions cannot be cleared through remount,
attached or detached mount attributes; bind/namespace/propagation copies retain
these immutable floors. Unbindable roots and nonrecursive copies hiding locked
children are rejected before copying. Shared record materialization takes an
explicit destination rather than selecting a current task namespace.

`open_tree_attr` NAMESPACE no-op attributes return the namespace fd. Non-noop
attributes are validated then rejected with EINVAL because the namespace inode
is not a mount root, rather than changing the cloned tree. Successful fsmount
consumes creation parameters before namespace/fd publication and retains a
clean reconfiguration view under its existing serialized context lock.

Validated: kernel host 2630; period45 Python657 (3 existing environment skips),
Rust6067 (1 existing ignore); q35/n305 lint (784 existing kernel warnings);
KVM system 70/70 (`system-ggdc211s`); paired mount-api/fsattrs 19/19 selected
contracts (`abi-gx2ysi0v`). Real children enter selected roots, see only the
selected file/tree, retain a recursive nested tmpfs after source unmount, and
cannot reach the old proc root. Genuine nsfs identity/type/CLOEXEC, ENOTDIR,
unbindable rejection, and fsmount context consumption pass on both guests.

The first ABI attempt stopped at the declaration gate (fixed in commit44),
not at a guest. The next Linux run exposed a test incorrectly reusing a consumed
fsmount context; both the test and native phase consumption were corrected.
Initial compile-only import/type/assert formatting errors were repaired before
this final run. Neither failed attempt is recorded as acceptance.

#### Known differences and validation scope

- Existing read-only construction imposes a provider readonly floor even on
  some clones backed by writable superblocks. Clearing RO can remain
  EOPNOTSUPP rather than Linux success. This is not relaxed as a side effect
  of namespace bring-up, and the corresponding cells remain partial.
- Existing fsconfig binary/non-overlay path consumption and CREATE_EXCL gaps
  remain. Consumed detached-context reconfigure still uses current's mount
  ledger. These are not mistaken for working backend reconfiguration.
- Cross-owner floors/one-way peers and rollback have host coverage. FUSE/NFS
  namespace teardown and concurrent fault injection were not exercised in
  paired guests; no physical hardware acceptance is claimed.

## Tool acceptance ladder

1. util-linux unshare/nsenter: pending.
2. bubblewrap read-only bind/tmpfs isolation: pending; requires level 1.
3. crun busybox OCI bundle and actual memory/pids enforcement: pending; requires level 2.
4. offline rootless podman with `--network=none`: pending; requires level 3.

No pending level is claimed usable. Tools and images will use a separate signed
Alpine payload, not the default guest filesystem.
