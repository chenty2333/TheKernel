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
- Direct non-mount installed-admin and PID ancestry checks are now fixed.
- The visible-root NEWUSER admission discrepancy is now fixed and the
  combined raw creation/NNP cases require successful native creation.

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
contracts (`abi-8gos6_qh`). Plain native NNP children observe NNP=1, cannot clear it, and leave the
parent unchanged. The combined NEWUSER raw branch accepted EPERM; it did not
prove successful native NEWUSER creation. Only the host credential constructor
established combined NEWUSER+NNP at this stage. The stronger setns fixture
below exposed the missing native creation admission, to be fixed next.

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

### Non-mount `setns` installed authority and PID ancestry

Target-owner authority is not interchangeable with installed-credential
CAP_SYS_ADMIN: ownership of a descendant user's UTS namespace can grant the
former even after the caller drops the latter. Direct non-user descriptors
now require both; pidfd sets check the installed domain unless NEWUSER prepares
that credential (the existing post-transition check then applies). A direct
PID namespace outside the active namespace's descendant tree returns EINVAL.
No ptrace permission policy was changed.

Validated: kernel2631, q35 lint784, system70/70 (`system-1en50sm7`), paired
task-control14/14 (`abi-309zpcq6`). A real unshare-created user/UTS target has a
distinct hostname; direct and pidfd positive controls enter it, while dropping
only the caller's admin denies entry and leaves its hostname unchanged. A real
PID-namespace init cannot select its parent's PID namespace. Direct time setns
still lacks the single-thread EUSERS gate and remains child-clock-only, rather
than Linux's active+child transition; this separate limitation is retained.

The first stronger fixture used clone3 NEWUSER+NEWUTS and exposed native EPERM
from the incorrect layered-root chroot guard, before reaching setns. It was
not counted as passing. Using ordinary fork+unshare isolates the setns fix;
the clone3 cell is conservatively partial again until the next root-identity
fix. Earlier combined NEWUSER+NNP acceptance claims are corrected above.

### NEWUSER admission follows the visible namespace root

An ordinary root filesystem layered on the immutable namespace underlay is
not a chroot. Both clone and unshare now compare the coherent fs_struct root
with the namespace's visible root by retained mountpoint and dentry identity,
under the existing namespace-operation/publication order. Numeric inode
identity alone and the underlay's `is_root` marker are insufficient. A real
restricted directory root remains EPERM; unshare now enforces that missing
restriction rather than silently creating authority inside a chroot.

Validated: kernel2631 (layered root, restricted entry, and same-inode foreign
mount identity); q35 lint784; system70/70 (`system-4pxqympe`); paired task-control
and sysadmin20/20 (`abi-7ovmhlp9`). NEWUSER+NNP and NEWUSER+UTS require real
successful native children now, not the old optional EPERM branch. Both clone
and unshare reject the actual chroot fixture on both guests. This closes the
creation gap exposed in step46 and restores clone3's implemented status.

### Modern mount create authority inside user namespaces

The first real isolation test reached a tmpfs mount failure in signed
util-linux. A bounded comparison confirmed full child CapEff/CapPrm and
successful BusyBox legacy mount, but libmount's modern create path returned
permission denied (`shell-nzcl1mr2`). The cause was fsconfig CREATE checking
initial-user privilege for every filesystem, ignoring Linux FS_USERNS_MOUNT.

Creation now chooses the supported filesystem's Linux authority domain and
retains fsopen's creator namespace rather than substituting current after a
namespace change. Non-userns filesystems still require initial-user privilege;
fspick cannot reach CREATE in its reconfiguration phase, so its creator field
is not claimed to identify a superblock owner.

Validated: kernel2632, q35 lint784, system70/70 (`system-6cxpi5ya`), paired
mount-api11/11 (`abi-f_hk3zg9`). Actual private tmpfs creation succeeds; an
inherited parent-owned context still fails EPERM after user/mount unshare,
and privileged-only hugetlbfs remains denied. Both real BusyBox and modern
util-linux mounts now return0 (`shell-1qwhe8h3`). The broader isolation script
advanced through mounting and parent isolation, then exposed missing
`/proc/<wrapper>/ns/pid_for_children`; its timeout was not acceptance. Add the
real child-PID descriptor next, and make failed script cleanup release its
namespace init before waiting. Restricted proc/sysfs visibility beyond the
existing provider behavior has not been separately established here.

### Level 1: real unshare/nsenter accepted

`/proc/<pid>/ns/pid_for_children` now selects that target task's retained
child PID namespace, independently of its active PID namespace. It is a real
nsfs object with the standard `pid:[inode]` label, PID type/parent/owner ioctls
and setns grammar, not a fabricated link to the active PID namespace.

Validated: kernel2632, Python7 related tests, q35 lint784, system70/70
(`system-x6i3qwxp`), paired task-control15/15 (`abi-55oqv1a4`). The raw probe
establishes equal active/child identity initially, distinct child identity after
PID unshare without changing active identity, and the real label/type ioctl.

The optional signed guest regression `container-namespace.sh` runs exact
`unshare -mpfUr --mount-proc`, verifies PID1 and mapped root IDs, excludes a
live outside top process from both ps views, mounts/writes private tmpfs, and
proves the parent's mount graph/file view and original namespace unchanged.
Real nsenter enters the other task's user/mount/child-PID namespaces and reads
the private marker. It emits KTAP and exits0 (`shell-v6lsjj09`). Cleanup releases
the namespace init normally before waiting on its wrapper; the earlier missing
path timeout is not counted as success. Level 1 is now usable; proceed to bwrap.

### Separate signed container payload

`--toolchain containers` selects a separate384MiB image; the default128MiB
rootfs and baseline init/accounts remain unchanged. The builder verifies the
exact131-package Alpine3.24.1 closure, disables installer scripts/triggers and
host ownership changes, and retains runtime licenses. It stages bwrap0.12.0,
crun1.30.1, podman5.8.8, conmon, uid/gid-map and FUSE helpers without launching
any of them as host containers. The repeated signed build stages109MiB.

A minimal dynamic BusyBox OCI root includes its loader and bind destinations.
An offline `alpine:3.24.1`/`alpine:latest` image comes from the pinned release
archive; manifest/config/layer digests were validated without registry pulls.
No cgroup/OCI/podman runtime acceptance is implied by packaging.

Validated: related Python5 and period50 full host663/Rust6069, system70/70
(`system-0pnpux24`), q35/n305 lint784. Real bwrap --version reports0.12.0.
The initial readonly fixture target was absent and has been added. Bwrap now
reaches pivot cleanup and reports `unmount old root: Invalid argument`
(`shell-etbfzsfw`); level2 remains pending. Linux transfers the old root's
placement lock to the new root when pivoting; repair this observed lifecycle
before attempting crun. The noninteractive bwrap script is staged, but its
success marker has not occurred and is not recorded as passed.

### Level 2: pivot placement custody and real bwrap accepted

The observed old-root unmount EINVAL came from retained placement custody:
Linux pivot transfers MNT_LOCKED to the new visible root and clears it on the
old root. VFS now performs that sole structural transfer under its tree writer
at the infallible edge-swap boundary, with no public unlock API. The prepared
namespace ledger mirrors both mounts' placement state atomically. Attribute
floors remain with their own mounts; pivot does not remove those restrictions.
A locked new pivot root remains inadmissible.

Validated: affected VFS and kernel2632 host tests; q35 lint784; system70/70
(`system-yh57drsd`); paired mount-api11/11 (`abi-d3v_3u5r`). The real mapped-user
namespace fixture cannot detach the new protected root but can detach/rmdir its
old root after pivot. The initial Linux fixture omitted uid/gid mappings and
failed mkdir; it was corrected, not counted as a kernel pass. VFS tests also
establish old-root detach while retaining the new boundary lock.

Real signed bwrap0.12.0 now runs the minimal BusyBox shell/commands, refuses
writes to readonly root/fixture, writes private tmpfs, changes hostname, and
has distinct user/mount/PID/net/UTS/IPC identities. It emits KTAP and exits0
(`shell-g1g1ef7d`). Level2 is accepted; proceed to crun and real cgroup limits.
The formal runner's actual staged loader/fixture/offline image were verified;
an earlier manual staging-tree assertion used a stale tree, not the runner's
payload, and is not a claim of an absent guest loader.

## Tool acceptance ladder

1. util-linux unshare/nsenter: passed, noninteractive signed-tool guest regression.
2. bubblewrap read-only bind/tmpfs and six-namespace isolation: passed.
3. crun busybox OCI bundle and actual memory/pids enforcement: pending; requires level 2.
4. offline rootless podman with `--network=none`: pending; requires level 3.

No pending level is claimed usable. Tools and images will use a separate signed
Alpine payload, not the default guest filesystem.

### Anonymous executable memfds (level 3 preparation)

The initial crun probe exits1 before its version or OCI command, with its generic
self-cloning error (`shell-y9lv5eno`). Its upstream1.30.1
[self-cloning implementation](https://github.com/containers/crun/blob/1.30.1/src/libcrun/cloned_binary.c)
can use either a readonly bind fd or sealed memfd. An independent sealed-ELF
probe exposed ETXTBSY through the initial writable memfd OFD, which incorrectly
owned a pathname writer lease. This is a real fallback defect, not yet proof
that it was crun's selected branch.

Memfd creation now prepares a zero-link inode on a private kernel shmem mount,
with the original opaque name, real ownership, initial mode/seals and a reserved
unpublished descriptor. It never creates a caller-visible /tmp/memfd path or
consults chroot's directory tree. Like Linux alloc_file_pseudo(), its initial
writable OFD does not take pathname writer exclusion; ordinary writable reopens
still do. The existing cached backend preserves shared RW/RX mappings. Only
this exact private mount receives namespace-neutral flags/idmap treatment;
unknown ordinary mounts remain errors. ELF preflight reads the same cached
bytes as image mapping, rather than stale lower-inode data.

Validated: kernel2634 (anonymous inode/name/owner and dirty-header regressions),
q35 lint784, system70/70 (`system-jdt3qw10`), and the paired memfd-create program
including actual sealed ELF execveat and zero-link/name checks (`abi-dza2f1bk`). Earlier full
guests failed two existing futex/JIT tests: the first implementation omitted
private-mount stat handling, then selected a direct backend without shared-file
mmap. Those defects were repaired, not counted as passes. An ABI invocation
without its required KVM selector did not execute the oracle; the corrected
selected run passed. No crun/OCI/resource-limit acceptance is claimed yet.

### Retained executable identity and readonly mount-root execution

Crun's first successful readonly bind path returns a detached O_PATH mount-root
fd, not an ordinary File wrapper. Execveat now admits its actual root Location
and retained root idmap. Preflight consumes the same frozen VFS actor/Landlock
context, preserving the caller's current tree for interpreter lookup while
adding the detached root's idmap. It does not make unknown descriptors into
executable files or treat the fd's display path as a lookup target.

Process runtime retains the terminal executable Location through fork/exec and
releases it on final exit. Proc exe is a real magic link to that object, under
the existing ReadFs image-access checks and exec/image revalidation. Readlink
uses display text plus the actual zero-link deleted suffix; reopening follows
the inode, not that text. This matters after memfd CLOEXEC and after detached
file-root execution, when no namespace pathname names the executable.

Validated: kernel2635, q35 lint784 (one new Option-replace style warning was
removed), KVM system70/70 (`system-xofd3d4a`), paired memfd-create/mount-api12/259
(`abi-au_sn7nb`). Raw children actually execute a readonly detached file root
and reopen its readonly regular proc exe inode, and reopen the fully sealed
zero-link memfd after CLOEXEC. Real signed crun now reports version1.30.1 and
its feature set (`shell-1lvje7a9`); this is startup acceptance only, not OCI run
or cgroup limits. The next observed gap is the absent sysfs cgroup mount point.

Known differences: detached file-root display text can collapse to `/`, and
ordinary proc exe display text is the saved exec path, not a full Linux d_path
rename/root projection. The retained inode, sealing and readonly state were
tested; detached FUSE/NFS executable-provider retirement was not. No time
namespace or deferred B1 field work is folded into this repair.
