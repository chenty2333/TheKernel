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
3. crun busybox OCI and actual memory/pids enforcement: accepted by optional
   signed KTAP4/4 (`shell-ujtgjal5`); advanced controller boundaries below remain.
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

### Creation filesystem IDs and period 55

The new anonymous memfd path initially selected effective UID/GID, whereas
Linux's shmem inode owner comes from the creating task's filesystem UID/GID.
It now selects fsuid/fsgid from the same pinned credential. The paired raw
fixture keeps effective IDs0 while setting filesystem IDs123/456 and requires
the actual inode to report123/456; it runs in isolated guest children, not by
changing host identities. Both guests passed (`abi-jibfcrev`).

The complete period55 passed: Python663 with3 existing environment skips,
Rust6073 with1 existing ignore (kernel2635), q35/n305 lint784, and KVM system70/70
with no skip/normal shutdown (`system-co_rgp_c`). The existing Linux-excerpt
inventory checks still pass; no baseline widening or invented provenance
repair was needed. Next: expose the cgroup mount point and run the actual OCI
bundle, then enforce/test its real memory and pids limits before podman.

### Sysfs cgroup mount point

Sysfs now declares `/sys/fs/cgroup`, matching Linux's cgroup mount-point
registration. It remains an immutable empty mount point until userspace mounts
the real cgroup filesystem; no fake controller files or boot-mounted hierarchy
were added. The existing host mount-point test covers it.

Validated: kernel2635, q35 lint784 and KVM system70/70. The real OCI probe now
reaches cgroup control-file lookup, but cat and crun fail to open
`cgroup.controllers` with EOPNOTSUPP (`shell-8ptpcdx2`, crun exit1). This is not
OCI acceptance: CgroupFile/CgroupDir do not currently provide persistent inode
userdata for the OFD errseq interface. Repair that actual file-open boundary
next, then rerun the bundle. Memory is still not advertised or enforced.

### Cgroup OFD errseq integration

Cgroup file and directory nodes now own persistent inode userdata. The existing
VFS errseq interface can therefore construct real OFDs instead of rejecting
open with EOPNOTSUPP. Repeated aliases of one inode share its state; distinct
inodes do not. Controller contents, authority checks and pids admission are not
replaced with a userspace shim or synthetic data.

Validated: kernel2636 with the new inode-identity regression; q35 lint784;
KVM system70/70 (`system-7nh_4t_r`); selected fs-abi24/259 (`abi-38hsmr9g`, generic
open/read/descriptor regression, not a cgroup-specific oracle assertion).
The real mount explicitly returns0 and cat prints the genuine `pids` controller
(`shell-n70c_dx3`). Crun advances but exits1 opening `self/setgroups` relative to
its detached fsmount proc-root fd: ENOTDIR. Upstream1.30.1's get_procfd prefers
that new mount API; the kernel's directory-fd helper currently recognizes only
its Directory wrapper. Fix that observed path capability next. OCI, memory and
pids-limit acceptance remain pending; rootless podman has not been attempted.

### Detached directory path capabilities

Directory-fd resolution now accepts an actual directory-root FsMountFd while
retaining its original OFD and provider-tree custody. Mount descriptors publish
real O_PATH status: they support pathname lookup, not read/getdents data access.
Legacy openat captures its exact dirfd once and keeps it through provider work;
openat2 uses its existing retained description. Both carry the root's idmap
into VFS authority, and an unscoped absolute path still ignores dirfd.

Validated: kernel2637 (same OFD identity/tree custody and O_PATH data refusal),
q35 lint784, KVM system70/70 (`system-b5xv2ge7`), selected
mount-api/fd-lifecycle/fs-abi41/259 (`abi-38hfxsm5`). The raw detached tmpfs root
supports openat creation and bounded openat2 read, reports O_PATH, and refuses
getdents with EBADF. The first new fixture unnecessarily tried unlink cleanup;
Linux returned ENOENT before candidate execution. The fixture now closes its
sole private mount fd to dispose the whole tmpfs. That failed assertion is not
counted as passed, and this regression does not establish unlinkat or distinct
idmaps on recursively detached submounts.

Actual crun now passes detached-proc reads, enters the six configured namespace
classes and reports the real child PID, but exits1 because
`/proc/sys/kernel/cap_last_cap` is absent (`shell-rg_ed0t0`). OCI command/exits and
resource limits remain pending; add that real capability-bound node next.

### Rebuilding locked signed APKs after repository-index updates

The live Alpine index advanced libseccomp to2.6.1-r0 and stopped offering the
locked2.6.0-r2, although its authentic signed APK remained in the existing
cache. The builder now supplies exact cached APK paths as explicit solver
candidates alongside every unchanged version constraint. Ambiguous cached
sources fail closed; signature verification and exact installed-closure checks
remain mandatory. No --allow-untrusted, host scriptlets or version widening
were added.

Validated: related Python6 and shell syntax; repeated signed131-package build
(110.8MiB installed/109MiB staged), formal containers guest restaging and actual
crun startup (`shell-agb7gx_p`). This was a packaging failure before guest
execution, not a kernel ABI failure. The capability-node candidate is still
uncommitted; the runtime now gets beyond that read and reports unsupported
SIOCSIFFLAGS, not OCI acceptance.

### Real capability ceiling required by OCI capability setup

`/proc/sys/kernel/cap_last_cap` is a read-only node rendered from the same
`CapabilityNumber::MAX` that validates credential admission (40, matching the
Linux7.2.3 capability UAPI). It is not a guessed runtime capability count.
The formatter/admission boundary has a host regression. Full period60 host,
KVM system70/70 (`system-intdfb4t`) and q35/n305 lint passed. Actual signed
crun1.30.1 now completes this read and advances to loopback bring-up, where
SIOCSIFFLAGS still returns EOPNOTSUPP (`shell-agb7gx_p`, exit1). This establishes
neither OCI command execution nor cgroup resource-limit acceptance.

### OCI loopback activation through SIOCSIFFLAGS

Socket/packet ioctl now imports the real short flag member and checks the
captured caller's CAP_NET_ADMIN against the socket's retained network owner.
Name selection and UP mutation use one router service permit, including real
route-generation/wake publication; no success-only stub is used. Volatile
LOOPBACK/RUNNING input bits cannot overwrite device facts. Other mutable flag
policy changes and SIOCSIFMTU remain unsupported, not silently accepted.

Validated: linux-net42 and kernel2639 host tests; q35 lint784; KVM system70/70
(`system-ttj_g7n5`); paired network-basic4/259 (`abi-uk5hsr0c`). A fresh user/net
namespace really transitions lo down/up/down, leaves the input ifreq unchanged,
and preserves the parent's link state; a retained parent socket cannot be
mutated with child-namespace authority, and missing-device lookup follows the
capability gate. Actual signed crun now passes loopback activation but fails
at `umount2 oldroot: ENOENT` (`shell-bbqx4e79`, exit1). OCI command/resource
acceptance remains pending; fix that actual pivot/detach path next.

### Repeated old-root detach after pivot

The first real crun old-root detach committed; its second detach failed during
`.` search because the unobserved security walker re-resolved legacy current
mount membership, instead of using its already-captured VFS authority. Both
follow/no-follow unobserved walkers now use that frozen actor/idmap/Landlock
view. Search/DAC and the later mount-namespace membership gate remain intact:
a live detached cwd can resolve, but cannot be unmounted again (EINVAL).
No errno translation, fabricated mount record or tool-specific path rule was
added. Detached cwd with previously attached nonidentity idmaps is not covered
by this regression; their full custody remains a known boundary.

Validated: kernel2640 and lint784; KVM system70/70 (`system-geigxbvd`); paired
mount-api11/259 (`abi-fi28dwjh`) now executes saved-old-root fchdir, private
propagation, successful lazy detach and mandatory second-detach EINVAL, while
preserving the new-root inherited lock. Initial guest build failed only on the
new fixture's missing MS_REC definition; repaired, not counted as acceptance.
Temporary tracing was removed before validation. Actual signed crun1.30.1
executes the BusyBox OCI shell, prints CRUN_REAL_HELLO, uid=0/gid=0 and its real
cgroup namespace view 0::/, then returns0 (`shell-ujbpbqhv`). Basic OCI startup,
command and exit are now established. True memory/pids resource acceptance is
still pending; rootless podman must wait for those checks.

### Automated OCI lifecycle and real pids resource policy

The optional `container-crun.sh` regression is staged only with the separate
containers payload. It runs a real BusyBox OCI command as PID1/root with its
own hostname, then another real crun container configured with pids.limit=8.
The native probe retains seven simultaneously live children and requires the
next fork to fail with EAGAIN; it releases/reaps every child. Kept runtime
state lets the external test verify actual pids.max=8, pids.current=0 after
exit and pids.events max>=1, then explicitly delete both runtime states.
This is not merely checking that a limit write was accepted.

Validated: host static C build, shell syntax and related Python7; lint;
signed crun guest KTAP2/2 and test exit0 (`shell-m45k7dv7`). An initial test
fixture called an unstaged hostname symlink; the corrected fixture uses the
already-present BusyBox applet and reports failure logs before cleanup.
The first script exit1 is not counted as passed. Memory charge/OOM remains
unimplemented/unadvertised, so Level3 as a whole is still incomplete and
podman has not been attempted. This pids test covers fork, not all thread
accounting combinations.

### Physical allocation lifetime admission boundary

The generic page allocator now has one optional, immutable accounting-hook
slot for VirtMem/PageCache reservations. Admission happens after physical
reservation and outside allocator locks, before frame/usage publication. A
denial returns that exact allocation and reports NoMemory; final physical
return invokes retirement once. VMA removal, fork aliases and deferred pins do
not falsely refund pages which still have a physical owner. Heap/DMA/global
allocations are not intercepted, avoiding recursive metadata admission.

Validated: axalloc host1 exercises actual aligned allocator storage, accepted
anonymous/cache frames, exact rejection rollback, unchanged allocator usage,
final refunds and excluded Global pages; lint and KVM system70/70
(`system-7pot597d`). The first host fixture violated the existing bitmap's
1GiB base alignment requirement; corrected its storage alignment, not the
allocator's capacity assertions. No kernel policy hooks are installed yet,
so this is the independently validated ownership boundary, not memory.max or
OOM acceptance. Next connect cgroup hierarchy budgets to these real lifetimes.

### Enforced resident memory and scoped OOM

Cgroup-v2 now advertises pids/memory; v1 stays pids-only and cpu/io are not
advertised as placebo controls. Non-root enabled memory groups expose
memory.max/current/peak/events/events.local. Real VirtMem/PageCache physical
reservations own identity-bound charge tokens; shared/COW aliases do not
multiply them, and deferred pins/cache owners refund only at physical return.
An immutable v2 ancestor chain and one budget gate enforce every ancestor
atomically. Sparse mmap alone consumes no resident quota. Root groups remain
unlimited and do not expose these files, matching Linux7.2.3 CFTYPE_NOT_ON_ROOT.
Controller token batches validate before publication; disabling resets hidden
limits without inventing refunds. OOM denies the physical reservation and
queues uncatchable SIGKILL to that exact allocating member outside charge
locks, at most once per process identity. The seven Linux7.2.3 event keys report
real max/oom/kill counters; disabled low/high/group-kill/socket-throttle classes
remain zero because no such policy is active.

Actual signed crun KTAP4/4 passed (`shell-ujtgjal5`): OCI lifecycle, seven live
children then fork EAGAIN at pids.limit8, a 512MiB sparse mapping under a32MiB
resident limit, real4MiB charge/refund (753664 -> 5021696 -> 827392 bytes), and a
256MiB touching worker killed with exit137 plus positive max/oom/oom_kill
counters and surviving parent/test shell. Every kept runtime is explicitly
deleted. Level3's required OCI, pids and memory acceptance is now established;
period65 full host Python666/Rust6083 (existing3 skips/1 ignore),
KVM system70/70 (`system-j8lx7h6m`), q35/n305 lint and selected mm-contracts18/259
(`abi-k0ybbs9n`) passed. Final victim-group event attribution additionally
passed kernel2643, lint and KVM system70/70 (`system-y00igtpa`). Quota max/oom
events belong to the limiting group; oom_kill belongs to the victim group
and propagates to ancestors, as in Linux7.2.3 mm/oom_kill.c. The ancestor-limit
host regression checks that distinction. The earlier crun leaf-limit path
is unchanged by this attribution correction. Proceed to offline rootless
podman after this commit.

Known boundaries: this resident controller accounts user anonymous/cache
allocations, not kernel heap/page-table/socket memory or memcg swap. Global
kernel/background allocations without a process owner are not attributed to a
container. Existing charges retain their original owner across migration;
shared-mm owner selection across different cgroups has not been compared with
Linux. Limit reduction below existing usage is enforced on subsequent charges,
not Linux's synchronous local reclaim; quota OOM chooses the allocator rather
than Linux badness scoring/reclaim. Peak is read-only, not Linux per-OFD reset;
low/high/min/oom.group/proactive reclaim are absent. Advanced disable/re-enable
charge reparenting and namespace-relative controller export are not fully
established. These do not turn the tested memory.max into a stored-only value.

### Real caller PID namespace at cgroup migration/read boundaries

The genuine rootless bootstrap first failed its positive self-PID cgroup.procs
write with ENOENT, including when attempted before dropping privilege. The
provider was treating user-visible numbers as kernel-wide process-table keys.
Positive IDs now resolve strictly in the caller's active PID namespace; zero
selects self, absent bindings report ESRCH, and a non-leader TID selects its
actual thread group. Production member output projects into the reader's PID
namespace and hides unseen identities. Existing credential/namespace checks
and retained process identity remain; there is no raw-number fallback.

Validated: kernel2644, lint784, KVM system70/70 (`system-jzq7oejk`), selected
fs-abi24/259 (`abi-qg4f9dhy`); actual signed crun OCI/pids/memory KTAP4/4 remains
accepted (`shell-t0v1xqt2`). The guest bootstrap now writes its positive PID,
reads that same visible member back, drops all real/effective IDs/groups to1000
and emits the real UID proof (`shell-1d1f1o06`). Podman still has not loaded an
image: that execution fails at exec with EEXIST. A separate root execution of
podman --version reports5.8.8 successfully. Its actual ELF has an83MiB PT_LOAD
span while the randomized interpreter hint can lie inside that main image;
next fix image placement, rather than attributing exec failure to privilege.
V1 tasks/thread-granular membership and fully pinned multi-read PID views are
not established by this process-directed regression.

### Whole interpreter envelope placement for large PIEs

The main executable is now mapped before selecting an interpreter gap. The
chooser uses actual PT_LOAD memory extents (including bss/gaps/nonzero origin),
power-of-two alignment and checked arithmetic, then reserves a whole free
span below the reserved heap. It also validates the returned interval against
that boundary. No guessed executable-size ceiling, dropped bss, fixed larger
address or privilege-specific ELF rule was added. ET_EXEC interpreter mapping
keeps its fixed-address semantics; entry/AT_BASE use the actual mapped bias.

Validated: kernel2646 host tests (envelope/align/overflow/heap-boundary failure),
lint784, KVM system70/70 (`system-mkhr1ly5`), selected memfd-create/mount-api12/259
(`abi-_s0249vq`). Actual signed podman5.8.8 now starts from real UID1000 and
initializes its SQLite/overlay/crun configuration (`shell-fjripkcl`), rather
than exec EEXIST. It next fails its real CLONE_NEWUSER|CLONE_NEWNS reexec with
EBUSY; no offline image load or OCI hello acceptance is claimed yet. The
initial host fixture used the wrong xmas-elf p_type constructor; repaired,
not counted as passing. Next inspect that observed fork admission.

### Optional rootless offline regression staging

The separate containers payload now stages container-podman.sh. Guest setup
appends a UID/GID1000 account without replacing baseline accounts, supplies
real subordinate ranges, delegates a v2 subtree and places the bootstrap there
before dropping every real/effective ID/group. Podman is executed as that real
non-root user with overlay storage, an offline podman load and
run --rm --network=none --pull=never. Success requires both the actual UID proof
and a standalone hello line, not a runner exit code. No guest registry pull or
host user/container/service setup is introduced.

Related Python9 and shell syntax/lint passed; exact signed131-APK staging and
real UID1000 frontend startup were observed in shell-fjripkcl. This commits the
independent payload/regression wiring, not Level4 acceptance: actual namespace
reexec fails EBUSY before loading the offline image. Probe that live fork
blocker next; retain fail-closed pin/COW safety instead of suppressing it.

### Fork keeps writable file/bss holes lazy

Live stage diagnostics located the rootless namespace-clone EBUSY inside the
main writable file/bss COW mapping, not at pin admission or namespace authority.
The old clone path faulted every writable file-backed page under the parent mm
lock, including untouched ELF data/bss; cache pressure is an internal retry
from that fault path. Fork now copies only present private leaves through its
existing transactional COW/pin-aware machinery and leaves unfaulted holes lazy,
as in Linux7.2.3 mm/memory.c copy_pte_range. No pin fence, rollback assertion,
COW isolation or error check was weakened, and all temporary traces were removed.

Kernel2647 passed, including a real address-space/file-bss regression requiring
both parent and fork child to retain zero resident pages for untouched holes.
Actual UID1000 podman now creates its new user/mount namespace and invokes the
real signed newuidmap (`shell-vtrszzq0`); it fails that helper's ownership check
because /proc/<target> reports st_uid/st_gid0 instead of the target's1000.
No image load/hello acceptance yet. Period70 passed Python667 (3 existing
environment skips), Rust6087 (1 existing ignore), kernel2647, KVM system70/70
(`system-ex0m3af9`), q35/n305 lint and paired mm-contracts/task-control/memfd-create
34/259 (`abi-bx7w_jbl`). Next correct real proc inode ownership.

### Proc task directory ownership for subordinate-ID helpers

The signed newuidmap's target-owner check exposed proc PID directories reporting
static root-owned0755 metadata. Linux7.2.3 fs/proc/base.c task_dump_owner and
pid_getattr instead expose0555 task directories with live effective ownership,
including nondumpable credential transitions. SimpleDir now permits a live
metadata projection while preserving its exact provider type and stable inode;
ThreadDir delegates only that projection to a separate original module. Existing
proc visibility/stale-process checks and all other proc file access rules remain
unchanged; there is no ptrace or dumpability relaxation.

A host test changes effective IDs while retaining root real/fs IDs and checks
repeated metadata on the same inode. The genuine UID-drop guest helper also
checks both fstat on a retained proc directory and a fresh /proc/self stat after
the transition. Zombie directory ownership and caller-user-namespace stat ID
projection are not established by this focused fix. Kernel2648, related Python9,
lint784, KVM system70/70 (`system-tyet22g0`) and paired stat-access5/259
(`abi-_vhy_yuw`) passed. Actual UID1000 helper passed both ownership assertions
and signed newuidmap passed its target-owner check (`shell-sbw8cebb`), then failed
its capability reduction. The authenticated APK stores CAP_SETUID/CAP_SETGID
file-capability xattrs (not setuid modes); these are absent from staged helpers.
Next preserve those package attributes in the guest image, without privileged
host filesystem changes or relaxing capset.

### Preserve package file capabilities only inside the guest image

Alpine's signed shadow-subids4.18.0-r1 helpers use security.capability xattrs,
not setuid bits. Unprivileged APK staging cannot retain those host attributes.
The payload now extracts their exact authenticated package xattrs into runtime
installation metadata; a focused offline-image installer admits only the two
original single permitted/effective bits (CAP_SETUID or CAP_SETGID), writes them
with debugfs and reads the binary xattrs back before publishing the image.
No host setcap, chown, sudo, helper invocation, container or account modification
is performed. The standard payload signature/closure checks still apply.

Related rootfs Python18, shell/lint and actual signed131 staging passed. Offline
image readback verified both attributes. Actual UID1000 podman passed both real
mapping helpers and reexec, initialized overlay/SQLite, parsed the offline Docker
archive and reached layer extraction (`shell-8be4qa_8`). It now fails the archive
extractor's pivot-directory mkdir with EACCES; load/hello remains unaccepted.
Next diagnose that real rootless path/credential failure, not archive format.

### Mapped inode DAC overrides in a user namespace

Live mkdir diagnostics identified an owned0555 extraction directory (real
UID/GID1000) and a caller with all effective caps in its new user namespace.
The old DAC adapter nevertheless required initial-namespace capability authority.
Linux7.2.3 fs/namei.c generic_permission and kernel/capability.c instead check the
selected capability in the actor's own namespace plus BOTH inode IDs' mappings.
The live non-idmapped DAC path now captures that inode scope; normal mode/ACL
checks still run first, and unmapped UID or GID does not grant an override.
Frozen selected capability bits and ordered security capability dispatch are
both required. Synthetic real-ID snapshots and retained nonidentity mount-idmap
capability dispatch are not broadened by this fix; those boundaries remain.

Host tests cover mapped and separately unmapped UID/GID, a selected-capability
denial and the regular-file execute-bit restriction. A paired stat-access child
uses a real UID/GID1000 drop, self-mapped user namespace, positive owned-directory
mkdir and mandatory EACCES for both unmapped-owner negative fixtures. Temporary
runtime diagnostics were removed. Kernel2649, lint and system70/70
(`system-m8ie9xj1`) passed; the paired assertion registry initially lacked the new
marker and was corrected without weakening it. The Linux self-map fixture also
needed PR_SET_DUMPABLE after its UID drop; this was corrected in the test, not
proc access policy. Paired stat-access5/259 (`abi-fgg2cdxi`) and related Python7
passed, including the positive and both mandatory unmapped-ID denials.
Actual podman passes pivot-directory creation (`shell-te2rh1rc`) and now fails
the extractor's fallback chroot with EPERM; image load/hello remains unaccepted.
Next fix that specific own-user-namespace chroot capability gate.

### Own-user-namespace chroot authority for extraction

The real archive extractor falls back from pivot_root to chroot; the previous
chroot gate required initial-namespace CAP_SYS_CHROOT. Linux7.2.3 fs/open.c uses
own-user-namespace capability authority. The syscall still resolves the path,
checks directory type/search permissions and only then checks the frozen
selected CAP_SYS_CHROOT and ordered own-namespace security authority before
publishing the actual fs root. No other capability, root-boundary or namespace
admission check is relaxed.

Host coverage verifies a child namespace's authority and a denied frozen
selected-capability projection. The paired mapped-user child verifies the
actual new root's device/inode identity, then drops effective CAP_SYS_CHROOT
and requires EPERM on another chroot. Kernel2650, related Python7, lint784,
KVM system70/70 (`system-nfw7e541`) and paired stat-access5/259
(`abi-xvqft8gb`) passed. An initial ABI invocation used an unregistered program
filter; only the correctly selected invocation counts. Actual podman now enters
the extraction root and fails lchown of /etc/shadow with EPERM
(`shell-kwctgnq0`); load/hello is still unaccepted. Next address the actual
inode-scoped chown capability gate.
