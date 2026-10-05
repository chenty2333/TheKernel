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

- New mount namespaces from `open_tree`/`fsmount`, published through existing nsfs.
- `clone3` `CLONE_PIDFD_AUTOKILL` final-OFD teardown.

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

## Tool acceptance ladder

1. util-linux unshare/nsenter: pending.
2. bubblewrap read-only bind/tmpfs isolation: pending; requires level 1.
3. crun busybox OCI bundle and actual memory/pids enforcement: pending; requires level 2.
4. offline rootless podman with `--network=none`: pending; requires level 3.

No pending level is claimed usable. Tools and images will use a separate signed
Alpine payload, not the default guest filesystem.
