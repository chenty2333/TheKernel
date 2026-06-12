# Deferred Validation 2026-06-10

This is the pending-validation list for the 2026-06-10 code-first LTP sprint.
It records syscall and kernel-surface work that has been implemented statically
and should be replayed later.

## Rules

- This file is not pass evidence.
- Keep it short: syscall/surface names, short caveats, and replay batches only.
- Do not keep command logs, long implementation notes, or repeated build-check
  details here.
- Do not add `Cheap Checks Recorded`, `Cheap Verification Performed`, or similar
  sections. Format/build checks belong in the turn summary or campaign evidence.
- When cases are promoted or moved into a campaign taxonomy, remove the matching
  old items from this list.

## Static Surfaces To Replay

No LTP replay, campaign run, promotion, or score harvest is claimed here.

### IPC And Async I/O

- SysV message queues: `msgget`, `msgsnd`, `msgrcv`, `msgctl`,
  effective-credential permissions, `MSG_STAT_ANY`, `MSG_INFO`,
  FIFO dequeue scalability for `msgstress01`, `/proc/sysvipc/msg`,
  `/proc/sys/kernel/msg_next_id`.
- SysV shared memory: `shmget`, `shmat`, `shmdt`, `shmctl`,
  `SHM_STAT_ANY`, `/proc/sysvipc/shm`, `/proc/sys/kernel/shm_next_id`.
- SysV semaphores: `semget`, `semctl`, `semop`, `semtimedop`,
  `semtimedop_time64`, effective-credential permissions, `SEM_INFO` and
  `SEM_STAT_ANY` index consistency, `/proc/sysvipc/sem`,
  `/proc/sys/kernel/sem`, `/proc/sys/kernel/sem_next_id`.
- Direct file I/O: open-time `O_DIRECT` status propagation and shared
  512-byte logical-sector buffer/count/offset alignment checks across read/write,
  pread/pwrite, readv/writev, preadv/pwritev, and AIO-backed paths; cached/direct
  page-cache serialization including mmap fault population; open-time
  `O_SYNC`/`O_DSYNC` status propagation and post-write sync for regular-file
  write, pwrite, vector, sendfile, copy-file-range, and splice destinations.
- Eventfd overflow path used by AIO notification.

### Process, Pidfd, Ptrace, And Modules

- PID checkpoint/restore: `/proc/sys/kernel/ns_last_pid`.
- Pidfd: `pidfd_send_signal` proc-dir compatibility, non-pidfd `EBADF`, stale
  PID identity, `pidfd_getfd`, `pidfd_open` non-positive PID errno and
  readiness, `waitid(P_PIDFD)`.
- Cross-process memory/advice: `process_vm_readv`, `process_vm_writev`,
  `process_madvise`.
- Ptrace subset: `TRACEME`, `ATTACH`, `SEIZE`, `DETACH`, `CONT`,
  `SYSCALL`, `SINGLESTEP`, `KILL`, `PEEKTEXT`, `PEEKDATA`, `POKETEXT`,
  `POKEDATA`, `SETOPTIONS`, `GETEVENTMSG`, `INTERRUPT`, `LISTEN`.
- Process comparison/accounting: `kcmp`, `acct`.
- Resource limits: `getrlimit`, `setrlimit`, `prlimit64`, mandatory
  getrlimit copyout faulting, `RLIMIT_NOFILE`/`RLIMIT_FSIZE` behavior,
  `RLIMIT_NPROC` clone/fork `EAGAIN` with privilege exemptions,
  `/proc/<pid>/limits`, and `/proc/sys/fs/nr_open`.
- Kernel module failure surface: `init_module`, `finit_module`,
  `delete_module`.

### Signal, Time, Timer, Futex, And Scheduler

- Signal wait and directed queue behavior around queued `siginfo`.
- POSIX timers: `timer_create`, `timer_settime`, `SIGEV_NONE`,
  `SIGEV_SIGNAL`, basic `SIGEV_THREAD` registration clearing.
- Interval timers and sleep: `alarm`, `getitimer`, `setitimer`,
  `nanosleep`, `clock_nanosleep`, LoongArch time64 dispatch.
- Time validation: `times`, `clock_settime`, `utimensat`, `ppoll`,
  `pselect6`, `recvmmsg`, `sendmmsg`, `adjtimex` invalid-mode errno,
  `settimeofday` nullable time pointer and timezone validation.
- Timerfd: `timerfd_settime` flag admission for
  `TFD_TIMER_CANCEL_ON_SET` with absolute timers and time namespace offsets.
- Futex wait surfaces: `futex_waitv`, `FUTEX_WAIT_BITSET`,
  `FUTEX_WAKE_BITSET`, bitset validation, and `FUTEX_CLOCK_REALTIME`
  command gating.
- Membarrier: `MEMBARRIER_CMD_QUERY`, registration commands, exec mmap
  generation tracking.
- Scheduler attributes and proc/sysctl: `sched_getattr`, `sched_setattr`,
  `sched_getscheduler`, `sched_setscheduler`, `sched_getparam`,
  `sched_setparam`, `sched_rr_get_interval`, `/proc/sys/kernel/sched_rr_timeslice_ms`.

### Memory Management

- NUMA compatibility: `move_pages`, `migrate_pages`, `get_mempolicy`,
  process-wide range migration, `mbind`, `set_mempolicy`, `MPOL_MF_STRICT`
  resident-page mismatch handling, `MPOL_F_NODE` page-node lookup from
  process/range policy including interleave masks, cpuset nodemask filtering,
  `MPOL_F_MEMS_ALLOWED` cpuset masks, synthetic nodes under
  `/sys/devices/system/node`, `/proc/<pid>/numa_maps`,
  `/proc/<pid>/status` allowed lists.
- Residency and locking: `mincore` page-cache residency for file-backed
  mappings without a local PTE, `mlock`, `mlock2`, `mlockall`, `munlockall`,
  locked `mmap`, `brk`, `mremap`, `VmLck`, `smaps Locked/Rss`.
- Mapping validation: `munmap`, `mprotect`, `msync`, `madvise`
  `MADV_DONTFORK`, `MADV_DOFORK`, `MADV_WIPEONFORK`,
  `MADV_KEEPONFORK`.
- RSS accounting: `getrusage`, `wait4`, `waitid` child usage snapshots,
  zombie `/proc/<pid>/stat` visibility, SIGCHLD `SIG_IGN`/`SA_NOCLDWAIT`
  autoreap behavior.
- Swap control surface: `swapon`, `swapoff`, `/proc/swaps`,
  `SwapTotal`/`SwapFree` meminfo accounting, and swap-backed
  `CommitLimit`.
- Overcommit sysctls: `/proc/sys/vm/overcommit_memory`,
  `/proc/sys/vm/overcommit_ratio`, `CommitLimit` from `/proc/meminfo`, and
  writable anonymous `mmap`/`brk` rejection under heuristic/strict policies.

### Filesystem, VFS, And File I/O

- New mount API and unmount: `open_tree`, `mount_setattr`, `move_mount`,
  `umount2`.
- Bind and move mounts: legacy `mount(2)` `MS_BIND`, `MS_REC`, `MS_MOVE`,
  non-root `EPERM`, invalid source/type errno ordering, read-only remount
  `EBUSY`, bind remount flags including read-only bind views,
  stacked same-target bind records and topmost unmount/remount accounting,
  write/open/truncate/fallocate/timestamp/xattr enforcement for read-only bind views,
  propagation-type flag admission, shared-peer bind/rbind mount-event
  propagation, per-mount overlay scoping, and `/proc/mounts` records.
- Metadata and path syscalls: `statx`, `readlink`, `readlinkat`, `chdir`,
  `fchdir`, `chroot`, `openat2`, regular-file `STATX_DIOALIGN`,
  inode flag attributes, and `FS_IOC_ENABLE_VERITY`.
- Ownership/permissions: `setfsuid`, `setfsgid`, `chown`, `fchown`,
  `fchownat`, `lchown`, `chmod`, `umask` mode-bit masking,
  setuid/setgid clearing.
- Xattr: `setxattr`, `fsetxattr`, `lsetxattr`, read-only mount rejection,
  `getxattr`, `fgetxattr`, `lgetxattr`, `listxattr`, `flistxattr`,
  `llistxattr`, `removexattr`, `fremovexattr`, `lremovexattr`.
- Fcntl: `F_DUPFD`, `F_DUPFD_CLOEXEC`, POSIX/OFD locks, async owner,
  `F_NOTIFY`, `F_SETLEASE`, `O_PATH` command filtering.
- Pipe/FIFO: `pipe`, `pipe2`, poll readiness, bad user-pointer unwind,
  pipe capacity rounding, `PIPE_BUF`-sized atomic writes.
- Splice family: `splice`, `tee`, `vmsplice`.
- Copy and send paths: `copy_file_range`, `sendfile`, `readv`, `writev`,
  `preadv`, `pwritev`, v2 positioned I/O.
- Advisory I/O: `readahead`, `sync_file_range`, `posix_fadvise`.
- File growth and cache coherency: `growfiles`, `fsx-linux`, truncate and
  ftruncate extend/shrink paths, sparse reads, direct-I/O page-cache
  invalidation, partial-page truncate dirty-state preservation, shared
  `O_APPEND` serialization across cached handles, and `MAP_SHARED` writeback
  visibility.
- Lseek sparse-file ABI: `SEEK_DATA`/`SEEK_HOLE` EOF/no-data `ENXIO` handling
  for tmpfs and the generic regular-file fallback.
- Tmpfs capacity and allocation: `size=` mount limits, live `statfs` free
  blocks, `ENOSPC` on write/fallocate, preallocated writes on full tmpfs, and
  hole-punch block release for `fallocate05`/fill-FS style cases.
- Loop devices: `/dev/loopN`, `LOOP_CONFIGURE`, `LOOP_GET_STATUS64`,
  `LOOP_SET_STATUS64`, `LOOP_CHANGE_FD`, block-size/read-only ioctls,
  `/dev/loop-control` `LOOP_CTL_GET_FREE`/`LOOP_CTL_ADD`/`LOOP_CTL_REMOVE`,
  `BLKRRPART`, dynamic `/dev/loopNp*` and `/sys/block/loopN/loopNp*`,
  `/sys/block/loopN/*`.
- Quota control: `quotactl`, `quotactl_fd`, VFS quota state, XFS quota
  commands, and `Q_XQUOTARM` removal state.

### Fsnotify And Polling

- Inotify: `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`,
  watch lifecycle, `IN_IGNORED`, `IN_DELETE_SELF`, `IN_UNMOUNT`,
  `IN_Q_OVERFLOW`, fdinfo masks, event record alignment, and `FIONREAD`.
- Fanotify: `fanotify_init`, `fanotify_mark`, normal events, permission
  events, parent notifications, partial FID/name/TID admission,
  `FAN_REPORT_PIDFD` event info records, fdinfo mark rendering, and
  ignore-mask visibility.
- Epoll: `epoll_ctl`, `epoll_wait`, `epoll_pwait`, always-polled
  `EPOLLERR`/`EPOLLHUP`, self-target errno ordering.

### Procfs, Sysfs, Identity, And Security Control

- Proc/sysctl surfaces for IPC, AIO, mqueue, keys, scheduler, UTS, nr_open,
  PID, namespace, and time namespace state.
- Namespace fds and ioctls: `unshare`, `setns`, UTS/time/PID/user/mount
  namespace fd identity, `unshare(CLONE_NEWNS)`, PID-parent visibility for
  `NS_GET_PARENT`, `NS_GET_USERNS`, `NS_GET_NSTYPE`, `NS_GET_OWNER_UID`
  owner copyout.
- Time namespace: `/proc/<pid>/ns/time`,
  `/proc/<pid>/ns/time_for_children`, `/proc/<pid>/timens_offsets`,
  `clock_gettime`, `timerfd`, `sysinfo`, `/proc/uptime`.
- UTS/syslog: `uname`, `gethostname`, `sethostname`, `getdomainname`,
  `setdomainname`, `/proc/sys/kernel/*`, `syslog`.
- Credentials and capabilities: `capget`, `capset`, supplementary groups,
  ambient capabilities, `securebits`, `keepcaps`, `/proc/status` capability
  rendering.
- Prctl: parent-death signal, subreaper, procfs `comm` line semantics,
  no-new-privs, timer slack, seccomp control plane, THP disable, capability
  bounding/ambient controls.
- Key management: `add_key`, `request_key`, `keyctl` read/update/revoke,
  per-keyring link/unlink/clear/replacement, describe/search/chown/security,
  instantiate/negate/reject, capability/persistent/restrict/move commands,
  encrypted-key payload admission, keyutils-style `debug:*` request fallback,
  Linux-compatible request-key default destination fallback, root and non-root
  key quota enforcement, `/proc/key-users`, `/proc/sys/kernel/keys/*`.
- Cgroup compatibility: `/proc/cgroups`, `/proc/<pid>/cgroup`,
  `/proc/<pid>/cpuset`, `cgroup` and `cgroup2` mounts,
  `/proc/mounts` and `/proc/<pid>/mountinfo` cgroup option rendering from
  mount data/controller tokens,
  `tasks`, `cgroup.procs`, `cgroup.controllers`, `cgroup.subtree_control`,
  top-down controller availability, v2 no-internal-process and child-disable
  `EBUSY` rules, newline-name rejection, `clone3(CLONE_INTO_CGROUP)`,
  `CLONE_NEWCGROUP` open-time namespace checks and open-time credential checks
  for `tasks`/`cgroup.procs` migration writes,
  recursive subtree `cgroup.kill`, `pids.max`, pids migration accounting,
  fork-time pids limit rejection, hierarchical `pids.current`,
  `pids.events` max increments on fork rejection,
  cpuset seed files, cpuset CPU/memory mask parsing and basic exclusivity flags,
  resident-backed memory usage/stat readouts, memory limit parsing,
  resettable memory max-usage and failcnt counters, `notify_on_release`
  inheritance, `release_agent`, `cgroup.clone_children` cpuset inheritance,
  v1 same-parent-only cgroup rename plus v2 rename rejection, `cpu.shares`,
  cpuacct usage/stat files, freezer state files, and common
  blkio/devices/net_cls/net_prio/hugetlb v1 controller knobs.
- TTY and PTY compatibility: `/dev/ptmx` lock/unlock, `/dev/pts` slave
  ownership and `0620` mode, slave-open gating, `FIONREAD`, `TIOCOUTQ`,
  line discipline get/set, `TCFLSH`, `TCXONC`, `TIOCGSID`, `TIOCVHANGUP`,
  and `vhangup` privilege gates.

### Networking

- Socket options and packet socket state: `setsockopt`, `getsockopt`,
  `SO_NO_CHECK`, unsupported TCP ULP, `PACKET_VERSION`, `PACKET_RX_RING`,
  `PACKET_RESERVE`, malformed `IPT_SO_SET_REPLACE`.
- Socket creation and protocol aliases: `socket`, `socketpair`, `accept4`,
  `SOCK_CLOEXEC`, `SOCK_NONBLOCK`, `IPPROTO_SCTP`, `IPPROTO_UDPLITE`,
  `SOCK_DCCP`/`IPPROTO_DCCP` netstress-compatible admission,
  `SOL_DCCP` service-code option validation, accept peer-address copyout
  before fd installation, AF_NETLINK packet wakeups, NETLINK_ROUTE
  bind/getsockname, common `SOL_NETLINK` toggles, ACKed route mutation
  messages, and multipart rtnetlink link/address/route dumps backed by shared
  `RTM_NEW*`/`RTM_DEL*` state.
- Tun device control surface: `/dev/net/tun` and `TUNGETFEATURES`.
- Send/receive errno paths: `sendto`, `sendmsg`, `sendmmsg`, `recvfrom`,
  `recvmsg`, `recvmmsg`, `MSG_OOB`, `MSG_NOSIGNAL`, oversized UDP
  `EMSGSIZE`, userspace buffer fault ordering, address copy-out length.
- Bind/connect/listen compatibility: invalid address/family ordering,
  fd lookup before sockaddr copy, UNIX pathname collisions, UDP `listen()`
  `EOPNOTSUPP`, AF_UNSPEC disconnect.

## Known Deep Deferrals

- Full fanotify FID/name event payloads and exact merge semantics.
- Full mount, IPC, PID, user, net, and cgroup namespace isolation.
- Real swap reclaim, swap cache, page-table swap entries, hugetlb, and cgroup
  memory accounting.
- Full module loader, ELF relocation, signatures, `/proc/modules`, and module
  dependency/reference tracking.
- Full ptrace register files, regsets, siginfo mutation, syscall/seccomp/fork
  trace events, and signal reinjection.
- LSM, idmapped mounts, exact user-namespace capability ownership, and
  filesystem-specific security/xattr behavior.
- Rich network subsystems such as NFS, full DCCP, IPsec, netfilter mutation,
  software TLS, and namespace-scoped packet capture. The unopened IPsec/VTI
  stress bucket requires real `NETLINK_XFRM` state/policy handling, XFRM packet
  transforms or correctly enforced bypass policy, and `vti`/`vti6` link devices;
  do not promote it on generic netlink ACKs alone.

## Replay Targets

Use the normal campaign flow after this code-first pass:

```bash
make lab-new NAME=<campaign> SUITE="<suite>"
make lab-run NAME=<campaign>
make lab-review NAME=<campaign>
make lab-apply NAME=<campaign>
make lab-done NAME=<campaign>
```

Prioritize replay in large semantic batches:

- IPC/AIO/direct-IO/eventfd/mqueue/semaphore.
- Pidfd/wait/ptrace/kcmp/acct/module/membarrier.
- Time/timer/scheduler/signal/futex.
- MM/NUMA/swap/mlock/mincore/mremap/getrusage.
- Xattr/fcntl/pipe/epoll/splice/copy_file_range/sendfile/vectored I/O.
- `growfiles`, `rwtest`, `doio`, and `fsx-linux` file-growth/cache-coherency
  families.
- `ltp-aiodio`, `direct_io`, and `dma_thread_diotest` stress families.
- Inotify/fanotify/mount/statx/loop/quota.
- `fs_bind`, `fs_readonly` `test_robind`, `mount`, `move_mount`, and
  `umount/umount2`.
- Credential/capability/prctl/securebits/UTS/syslog.
- Cgroup v1/v2 mount, hierarchy, pids controller, task migration, and
  kill-control smoke.
- PTY/TTY ioctl, `/dev/ptmx`, `/dev/pts`, and `vhangup` smoke.
- Socket option, bind/connect/listen, send/receive, packet, and UDP errno paths.
