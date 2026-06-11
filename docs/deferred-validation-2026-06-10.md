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
  `MSG_STAT_ANY`, `MSG_INFO`, `/proc/sysvipc/msg`,
  `/proc/sys/kernel/msg_next_id`.
- SysV shared memory: `shmget`, `shmat`, `shmdt`, `shmctl`,
  `SHM_STAT_ANY`, `/proc/sysvipc/shm`, `/proc/sys/kernel/shm_next_id`.
- SysV semaphores: `semget`, `semctl`, `semop`, `semtimedop`,
  `semtimedop_time64`, `/proc/sysvipc/sem`, `/proc/sys/kernel/sem`,
  `/proc/sys/kernel/sem_next_id`.
- POSIX mqueue: `mq_open`, `mq_unlink`, `mq_timedsend`,
  `mq_timedreceive`, `mq_notify`, `mq_getsetattr`.
- Linux AIO: `io_setup`, `io_destroy`, `io_submit`, `io_cancel`,
  `io_getevents`, `io_pgetevents`, `/proc/sys/fs/aio-*`.
- Eventfd overflow path used by AIO notification.

### Process, Pidfd, Ptrace, And Modules

- PID checkpoint/restore: `/proc/sys/kernel/ns_last_pid`.
- Pidfd: `pidfd_send_signal`, `pidfd_getfd`, `pidfd_open` readiness,
  `waitid(P_PIDFD)`, proc-dir stale PID identity.
- Cross-process memory/advice: `process_vm_readv`, `process_vm_writev`,
  `process_madvise`.
- Ptrace subset: `TRACEME`, `ATTACH`, `SEIZE`, `DETACH`, `CONT`,
  `SYSCALL`, `SINGLESTEP`, `KILL`, `PEEKTEXT`, `PEEKDATA`, `POKETEXT`,
  `POKEDATA`, `SETOPTIONS`, `GETEVENTMSG`, `INTERRUPT`, `LISTEN`.
- Process comparison/accounting: `kcmp`, `acct`.
- Kernel module failure surface: `init_module`, `finit_module`,
  `delete_module`.

### Signal, Time, Timer, Futex, And Scheduler

- Signal wait and directed queue behavior around queued `siginfo`.
- POSIX timers: `timer_create`, `timer_settime`, `SIGEV_NONE`,
  `SIGEV_SIGNAL`, basic `SIGEV_THREAD` registration clearing.
- Interval timers and sleep: `alarm`, `getitimer`, `setitimer`,
  `nanosleep`, `clock_nanosleep`, LoongArch time64 dispatch.
- Time validation: `times`, `clock_settime`, `utimensat`, `ppoll`,
  `pselect6`, `recvmmsg`, `sendmmsg`.
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
  synthetic nodes under `/sys/devices/system/node`.
- Residency and locking: `mincore`, `mlock`, `mlock2`, `mlockall`,
  `munlockall`, locked `mmap`, `brk`, `mremap`, `VmLck`, `smaps Locked/Rss`.
- Mapping validation: `munmap`, `mprotect`, `msync`, `madvise`
  `MADV_DONTFORK`, `MADV_DOFORK`.
- RSS accounting: `getrusage`, `wait4`, `waitid` child usage snapshots.
- Swap control surface: `swapon`, `swapoff`, `/proc/swaps`.

### Filesystem, VFS, And File I/O

- New mount API and unmount: `open_tree`, `mount_setattr`, `move_mount`,
  `umount2`.
- Metadata and path syscalls: `statx`, `readlink`, `readlinkat`, `chdir`,
  `fchdir`, `chroot`, `openat2`.
- Ownership/permissions: `setfsuid`, `setfsgid`, `chown`, `fchown`,
  `fchownat`, `lchown`, `chmod`, setuid/setgid clearing.
- Xattr: `setxattr`, `fsetxattr`, `lsetxattr`, `getxattr`, `fgetxattr`,
  `lgetxattr`, `listxattr`, `flistxattr`, `llistxattr`, `removexattr`,
  `fremovexattr`, `lremovexattr`.
- Fcntl: `F_DUPFD`, `F_DUPFD_CLOEXEC`, POSIX/OFD locks, async owner,
  `F_NOTIFY`, `F_SETLEASE`, `O_PATH` command filtering.
- Pipe/FIFO: `pipe`, `pipe2`, poll readiness, bad user-pointer unwind,
  pipe capacity rounding.
- Splice family: `splice`, `tee`, `vmsplice`.
- Copy and send paths: `copy_file_range`, `sendfile`, `readv`, `writev`,
  `preadv`, `pwritev`, v2 positioned I/O.
- Advisory I/O: `readahead`, `sync_file_range`, `posix_fadvise`.
- Loop devices: `/dev/loopN`, `LOOP_CONFIGURE`, `LOOP_GET_STATUS64`,
  `LOOP_SET_STATUS64`, `LOOP_CHANGE_FD`, block-size/read-only ioctls,
  `/sys/block/loopN/*`.
- Quota control: `quotactl`, `quotactl_fd`, VFS quota state, XFS quota commands.

### Fsnotify And Polling

- Inotify: `inotify_init1`, `inotify_add_watch`, `inotify_rm_watch`,
  watch lifecycle, `IN_IGNORED`, `IN_DELETE_SELF`, `IN_UNMOUNT`, fdinfo masks.
- Fanotify: `fanotify_init`, `fanotify_mark`, normal events, permission
  events, parent notifications, partial FID/name/TID admission.
- Epoll: `epoll_ctl`, `epoll_wait`, `epoll_pwait`, always-polled
  `EPOLLERR`/`EPOLLHUP`, self-target errno ordering.

### Procfs, Sysfs, Identity, And Security Control

- Proc/sysctl surfaces for IPC, AIO, mqueue, keys, scheduler, UTS, nr_open,
  PID, namespace, and time namespace state.
- Namespace fds and ioctls: `unshare`, `setns`, UTS/time/PID/user namespace
  fd identity, `NS_GET_PARENT`, `NS_GET_USERNS`, `NS_GET_NSTYPE`,
  `NS_GET_OWNER_UID`.
- Time namespace: `/proc/<pid>/ns/time`,
  `/proc/<pid>/ns/time_for_children`, `/proc/<pid>/timens_offsets`,
  `clock_gettime`, `timerfd`, `sysinfo`, `/proc/uptime`.
- UTS/syslog: `uname`, `gethostname`, `sethostname`, `getdomainname`,
  `setdomainname`, `/proc/sys/kernel/*`, `syslog`.
- Credentials and capabilities: `capget`, `capset`, supplementary groups,
  ambient capabilities, `securebits`, `keepcaps`, `/proc/status` capability
  rendering.
- Prctl: parent-death signal, subreaper, comm, no-new-privs, timer slack,
  seccomp control plane, THP disable, capability bounding/ambient controls.
- Key management: `add_key`, `request_key`, `keyctl`, `/proc/key-users`,
  `/proc/sys/kernel/keys/*`.

### Networking

- Socket options and packet socket state: `setsockopt`, `getsockopt`,
  `SO_NO_CHECK`, unsupported TCP ULP, `PACKET_VERSION`, `PACKET_RX_RING`,
  `PACKET_RESERVE`.
- Socket creation and protocol aliases: `socket`, `socketpair`, `accept4`,
  `SOCK_CLOEXEC`, `SOCK_NONBLOCK`, `IPPROTO_SCTP`, `IPPROTO_UDPLITE`,
  accept peer-address copyout before fd installation.
- Send/receive errno paths: `sendto`, `sendmsg`, `sendmmsg`, `recvfrom`,
  `recvmsg`, `recvmmsg`, `MSG_OOB`, `MSG_NOSIGNAL`, oversized UDP
  `EMSGSIZE`, userspace buffer fault ordering, address copy-out length.
- Bind/connect/listen compatibility: invalid address/family ordering,
  fd lookup before sockaddr copy, UNIX pathname collisions, UDP `listen()`
  `EOPNOTSUPP`, AF_UNSPEC disconnect.

## Known Deep Deferrals

- Full fanotify FID/name/pidfd event payloads and exact merge semantics.
- Full mount, IPC, PID, user, net, and cgroup namespace isolation.
- Real swap reclaim, swap cache, page-table swap entries, hugetlb, and cgroup
  memory accounting.
- Full module loader, ELF relocation, signatures, `/proc/modules`, and module
  dependency/reference tracking.
- Full ptrace register files, regsets, siginfo mutation, syscall/seccomp/fork
  trace events, and signal reinjection.
- LSM, idmapped mounts, exact user-namespace capability ownership, and
  filesystem-specific security/xattr behavior.
- Rich network subsystems such as NFS, IPsec, netfilter mutation, software TLS,
  and namespace-scoped packet capture.

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

- IPC/AIO/eventfd/mqueue/semaphore.
- Pidfd/wait/ptrace/kcmp/acct/module/membarrier.
- Time/timer/scheduler/signal/futex.
- MM/NUMA/swap/mlock/mincore/mremap/getrusage.
- Xattr/fcntl/pipe/epoll/splice/copy_file_range/sendfile/vectored I/O.
- Inotify/fanotify/mount/statx/loop/quota.
- Credential/capability/prctl/securebits/UTS/syslog.
- Socket option, bind/connect/listen, send/receive, packet, and UDP errno paths.
