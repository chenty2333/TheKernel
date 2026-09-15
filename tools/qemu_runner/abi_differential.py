"""Execute the structured portable ABI contracts on two equivalent guests.

This finite contract set is not an audit of the entire Linux syscall table.
"""
from __future__ import annotations

from collections import Counter
from dataclasses import dataclass
import os
import math
from pathlib import Path
import re
import shutil
import tempfile

from tools.product_state import validate_storage
from .boot_artifacts import validate_linux_boot, validate_linux_esp_kernel, validate_thekernel_esp_kernel
from .kernel_benchmark import BenchmarkTarget, SHELL_MARKER
from .model import Interaction, RunLimits
from .runner import RunConfig, RunnerError, run

COMPLETE_MARKER = "THEKERNEL_ABI_EXIT_ZERO"
# Explicit expectations prevent an accidentally deleted guest assertion from
# reducing acceptance coverage. Keep these aligned with tests/guest/portable.
CONTRACTS = {
    "tty-job-control": ("portable-differential", "pass", "AUTO_CTTY_NOCTTY SIGTTIN_SIGTTOU PTPEER_PACKET_HANGUP"),
    "tty-termios": ("portable-differential", "pass", "PYREPL_PREPARE_RESTORE CBREAK_SIGINT_FLUSH NOFLSH_PRESERVES_INPUT RAW_ESCAPE_CR_BYTES WINSIZE_SIGWINCH_NOOP"),
    "unix-write-credentials": ("raw-differential", "pass", "UNIX_SOCKET_IDENTITY PEER_PID_EFFECTIVE_IDS WRITE_SENDER_PID_REAL_IDS WRITEV_SENDER_PID_REAL_IDS SENDMSG_SENDER_PID_REAL_IDS CHILD_EXIT_CLEAN REAL_EFFECTIVE_IDS RIGHTS_RECEIVER_LIFETIME"),
    "eventfd": ("portable-differential", "pass", "LEGACY_FLAGS IO_ERRNO_STATE COUNTER_POLL SEMAPHORE CLOEXEC_TEARDOWN"),
    "creat": ("raw-differential", "pass", "PROVIDER_EXT4 CREATE_UMASK_STATUS TRUNCATE_EXISTING BAD_PATH_EFAULT TEARDOWN"),
    "time": ("raw-differential", "pass", "NULL_EPOCH_ERRNO UNALIGNED_EIGHT_BYTES CROSS_WRITABLE_PAGE EFAULT_COPYOUT REALTIME_BRACKET"),
    "umask": ("raw-differential", "pass", "PROVIDER_EXT4 MASK_AND_CREATE FORK_COPIES_FS CLONE_FS_SHARES UNSHARE_FS_SEPARATES EXEC_PRESERVES_FS"),
    "native-ni": ("fixed-slots", "enosys", "NR_134_USELIB NR_156_SYSCTL NR_174_CREATE_MODULE NR_177_GET_KERNEL_SYMS NR_178_QUERY_MODULE NR_180_NFSSERVCTL NR_181_GETPMSG NR_182_PUTPMSG NR_183_AFS_SYSCALL NR_184_TUXCALL NR_185_SECURITY NR_205_SET_THREAD_AREA NR_211_GET_THREAD_AREA NR_212_LOOKUP_DCOOKIE NR_214_EPOLL_CTL_OLD NR_215_EPOLL_WAIT_OLD NR_236_VSERVER OUT_OF_RANGE_1024"),
}

CONTRACTS.update({
    "setxattrat": ("raw-differential", "pass", "CREATE_REPLACE_EMPTY_PATH FIFO_USER_EPERM VALIDATION_ORDER"),
    "getxattrat": ("raw-differential", "pass", "PROBE_RANGE_VALUE FIFO_USER_ENODATA VALIDATION_ORDER"),
    "listxattrat": ("raw-differential", "pass", "PROBE_RANGE_NAMES VALIDATION_ORDER"),
    "removexattrat": ("raw-differential", "pass", "REMOVE_ABSENT_STATE FIFO_USER_EPERM VALIDATION_ORDER"),
    "file-getattr": ("raw-differential", "pass", "ALLOCATED_EXTENT_FIELDS_ZERO GET_EMPTY_PATH_ZERO_TAIL VALIDATION_ORDER"),
    "file-setattr": ("raw-differential", "pass", "NODUMP_IGNORES_INPUT_NEXTENTS EXT4_IGNORES_UNFLAGGED_HINTS ERRORS_PRESERVE_STATE RESTORE"),
    "open-tree-attr": ("raw-differential", "pass", "CLONE_CLOEXEC_DIRECTORY READONLY_CLONE_SOURCE_UNCHANGED VALIDATION_ORDER"),
})
CONTRACTS.update({
    "mprotect": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX_ONLY VALIDATION_RESTORE PROT_NONE_FORK_UNMAP_REUSE"),
    "munmap": ("raw-differential", "pass", "HOLE_NEIGHBORS_IDEMPOTENT VALIDATION_PRESERVES_NEIGHBORS"),
    "mincore": ("raw-differential", "pass", "TOUCHED_RESIDENCY_EXACT_OUTPUT VALIDATION_ORDER FILE_PAGE_RESIDENCY SHARED_MSYNC_FSYNC_REDIRTY LOCKED_SHARED_FSYNC"),
    "process-vm-readv": ("raw-differential", "pass", "EXACT_COPY REMOTE_FAULT_PREFIX VALIDATION_EMPTY_LOCAL REMOTE_CONTENT_CONFIRMED PERMISSION_EPERM"),
    "process-vm-writev": ("raw-differential", "pass", "EXACT_COPY REMOTE_FAULT_PREFIX VALIDATION_EMPTY_LOCAL REMOTE_CONTENT_CONFIRMED PERMISSION_EPERM"),
    "mseal": ("raw-differential", "pass", "VALIDATION_AND_MAPPING_SEAL DISCARD_RESPECTS_WRITE_PERMISSION"),
})
CONTRACTS.update({
    "network_bind": ("raw-differential", "pass", "IPV4_OVERLONG_EINVAL IPV4_STORAGE_BOUNDARY IPV6_OVERLONG_EINVAL"),
    "network_connect": ("raw-differential", "pass", "IPV6_OVERLONG_EINVAL IPV4_OVERLONG_EINVAL NETLINK_SOCKET NETLINK_KERNEL_CONNECT NETLINK_AUTOBIND NETLINK_DISCONNECT NETLINK_DISCONNECTED_PEER NETLINK_BAD_FAMILY TCP_CLOSE_QUEUED"),
    "network_getpeername": ("raw-differential", "pass", "NETLINK_UNCONNECTED_ZERO NETLINK_CONNECTED_ZERO NETLINK_PEER_POLICY NETLINK_PEER_STATE NETLINK_PEER_RESET NETLINK_TRUNCATED_LENGTH"),
    "network_sendto": ("raw-differential", "pass", "IPV4_OVERLONG_EINVAL IPV6_OVERLONG_EINVAL UDP4_ERROR_QUEUE UDP6_ERROR_QUEUE"),
})
CONTRACTS.update({
    "rt_tgsigqueueinfo": ("raw-differential", "pass", "COPY_BEFORE_INVALID_IDS INVALID_IDS_BEFORE_CODE COPY_BEFORE_INVALID_SIGNO"),
    "restart_syscall": ("raw-differential", "pass", "NO_PENDING_BLOCK_EINTR"),
    "rt_sigaction": ("raw-differential", "pass", "SIZE_BEFORE_COPY COPY_BEFORE_SIGNO INVALID_REPLACEMENT KILL_STOP_QUERY COMMIT_BEFORE_OLD_COPY"),
    "sigaltstack": ("raw-differential", "pass", "COMMIT_BEFORE_OLD_COPY BAD_NEW_PRESERVES ACCEPT_ONSTACK WRAPPING_GEOMETRY_STORED DISABLE_AUTODISARM OVERLAPPING_INPUT_OUTPUT INVALID_FLAGS_BEFORE_OLD_COPY"),
})
CONTRACTS.update({
    "access": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "faccessat": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "faccessat2": ("raw-differential", "pass", "MODE_BEFORE_PATH EXISTS_FLAGS"),
    "newfstatat": ("raw-differential", "pass", "PATH_FLAG_ORDER EMPTY_FD_IDENTITY NO_AUTOMOUNT_SYNC_FLAGS"),
    "statx": ("raw-differential", "pass", "EXTENSIBLE_MASK_VALIDATION EMPTY_FD_IDENTITY PROVIDER_OPTIONAL_FIELDS EXT4_OPTIONAL_FIELDS"),
})
CONTRACTS.update({
    "flock": ("raw-differential", "pass", "MANDATORY_BEFORE_FD_COMMAND COMMAND_BEFORE_FD VALID_COMMAND_BAD_FD"),
    "utimensat": ("raw-differential", "pass", "OMIT_BEFORE_PATH_FLAGS_FD COPY_BEFORE_FLAGS"),
    "fallocate": ("raw-differential", "pass", "FIFO_ESPIPE ACCESS_BEFORE_TYPE MODE_BEFORE_ACCESS_TYPE GEOMETRY_BEFORE_TYPE SOCKET_ENODEV"),
    "readahead": ("raw-differential", "pass", "PIDFD_EINVAL FD_BEFORE_OFFSET READ_PIPE_EINVAL ACCESS_BEFORE_TYPE_OFFSET PATH_FD_EBADF"),
})
CONTRACTS.update({
    "inotify_add_watch": ("raw-differential", "pass", "FD_BEFORE_MASK_CONFLICT MASK_BITS_BEFORE_FD MASK_CONFLICT_BEFORE_PATH"),
    "signalfd4": ("raw-differential", "pass", "COPY_BEFORE_FLAGS_FD SIZE_BEFORE_COPY FLAGS_BEFORE_FD VALID_MASK_FLAGS_BAD_FD"),
    "timerfd_settime": ("raw-differential", "pass", "COPY_BEFORE_FLAGS_FD FLAGS_BEFORE_FD VALUE_BEFORE_FD VALID_VALUE_FLAGS_BAD_FD"),
    "mlock": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "mlock2": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "munlock": ("raw-differential", "pass", "HOLE_COMMITS_PREFIX"),
    "process-madvise": ("raw-differential", "pass", "SELF_DESTRUCTIVE_ADVICE"),
    "mlockall": ("raw-differential", "pass", "POPULATE_FAILURE_IGNORED"),
})
CONTRACTS.update({
    "sched_getaffinity": ("raw-differential", "pass", "get-affinity get-unaligned-length get-low32-zero get-low32-length"),
    "sched_setaffinity": ("raw-differential", "pass", "set-low32-length set-low32-zero set-short-mask"),
    "getcpu": ("raw-differential", "pass", "getcpu-first-copy-fault getcpu-node-written-after-cpu-fault"),
    "sched_setparam": ("raw-differential", "pass", "setparam-negative-pid-before-copy setparam-null setparam-bad-pointer"),
    "sched_setscheduler": ("raw-differential", "pass", "setscheduler-negative-pid-before-copy setscheduler-negative-policy-before-copy setscheduler-positive-invalid-policy-after-copy setscheduler-null"),
    "sched_get_priority_max": ("raw-differential", "pass", "ext-priority-max"),
    "sched_get_priority_min": ("raw-differential", "pass", "ext-priority-min"),
})
CONTRACTS.update({
    "socket_msg.send_flags": ("raw-differential", "pass", "DONTROUTE_SENDTO CONFIRM_SENDTO COMBINED_SENDTO CONFIRM_SENDMSG DONTROUTE_SENDMSG FLAGGED_DATAGRAMS_DELIVERED"),
    "socket_msg.tcp_more": ("raw-differential", "pass", "TCP_MORE_LISTENER TCP_MORE_HELPER TCP_MORE_CONNECT MORE_SEND MORE_SENDMSG UNCORK_SEND TCP_MORE_JOIN MORE_STREAM_DELIVERED"),
    "socket_msg.compat_flag": ("raw-differential", "pass", "SENDMSG_COMPAT_EINVAL SENDMSG_COMPAT_MORE_EINVAL RECVMSG_COMPAT_EINVAL SENDMMSG_COMPAT_EINVAL RECVMMSG_COMPAT_EINVAL SENDMSG_BADF SENDMMSG_BADF RECVMMSG_BADF SENDTO_COMPAT_ACCEPTED"),
    "socket_msg.waitall_stream": ("raw-differential", "pass", "STREAM_MERGE_PAIR STREAM_MERGE_WRITER STREAM_MERGE_TEN STREAM_MERGE_DONE STREAM_EOF_PAIR STREAM_EOF_WRITER STREAM_EOF_SHORT STREAM_EOF_ZERO STREAM_EOF_DONE STREAM_EMPTY_PAIR STREAM_EMPTY_EAGAIN"),
    "socket_msg.waitall_tcp": ("raw-differential", "pass", "TCP_WAITALL_LISTENER TCP_WAITALL_HELPER TCP_WAITALL_CONNECT TCP_MERGE_TEN TCP_EOF_ZERO TCP_WAITALL_DONE"),
    "socket_msg.waitall_datagram": ("raw-differential", "pass", "DATAGRAM_SENT DATAGRAM_SINGLE_RECORD DATAGRAM_SENT_AGAIN DATAGRAM_PEEK_RECORD"),
    "socket_msg.recvmmsg_deadline": ("raw-differential", "pass", "EMPTY_EAGAIN EMPTY_TIMEOUT_UNCHANGED QUEUE_TWO PARTIAL_BATCH_COUNT PARTIAL_BATCH_PAYLOADS PARTIAL_BATCH_REMAINING QUEUE_TWO_AGAIN ZERO_TIMEOUT_ONE_DATAGRAM ZERO_TIMEOUT_STORED QUEUE_INVALID NONNORMALIZED_TIMEOUT_EINVAL TIMEOUT_BEFORE_BADF ZERO_VLEN_NO_BATCH ZERO_VLEN_TIMEOUT_UNCHANGED"),
    "socket_msg.recvmmsg_waitforone": ("raw-differential", "pass", "WAITFORONE_QUEUED WAITFORONE_ONE_DATAGRAM"),
    "prctl-name": ("raw-differential", "pass", "NAME_BYTES"),
    "prctl-timing": ("raw-differential", "pass", "TIMING"),
    "prctl-auxv": ("raw-differential", "pass", "AUXV"),
    "prctl-timer-restore-ids": ("raw-differential", "pass", "RESTORE_IDS"),
    "prctl-cfi": ("raw-differential", "pass", "CFI"),
    "arch_prctl": ("raw-differential", "pass", "SEGMENT_BASE_EPERM"),
    "iopl": ("raw-differential", "pass", "LEVEL_AND_LOWERING"),
    "capset": ("raw-differential", "pass", "PID_BEFORE_COPY"),
    "move_pages": ("raw-differential", "pass", "EMPTY_REQUEST_AND_FLAGS"),
    "modify_ldt": ("raw-differential", "pass", "DEFAULT_LDT WRITE_RULES"),
    "setpgid": ("raw-differential", "pass", "CHILD_OWN_GROUP EXEC_EACCES"),
    "clone3": ("raw-differential", "pass", "FLAG_ADMISSION NEWTIME AUTOREAP"),
    "epoll-membarrier": ("portable-differential", "pass", "EPOLLEXCLUSIVE_ADD_ADMISSION EPOLLEXCLUSIVE_ADD_BITS EPOLLEXCLUSIVE_MOD_BEFORE_LOOKUP EPOLLEXCLUSIVE_NESTED_EINVAL EPOLLEXCLUSIVE_DEL_IGNORES_MASK EPOLLMSG_INTEREST_ACCEPTED EPOLLWAKEUP_INTEREST_ACCEPTED MEMBARRIER_QUERY_MASK MEMBARRIER_FLAG_CPU_RULE MEMBARRIER_GLOBAL_COMMANDS MEMBARRIER_REGISTRATION_STATE MEMBARRIER_UNKNOWN_COMMAND RESTART_NO_BLOCK_EINTR NANOSLEEP_REM_WRITE_BACK NANOSLEEP_SA_RESTART_EINTR NANOSLEEP_RESTART_BLOCK_DEADLINE PPOLL_REMAINING_WRITE_BACK PPOLL_SA_RESTART_EINTR PPOLL_WITHOUT_RESTART_BLOCK SELECT_REMAINING_WRITE_BACK POLL_RESTART_BLOCK_DEADLINE"),
    "getcwd": ("raw-differential", "pass", "SIZE_ZERO_ERANGE NULL_BUF_EFAULT SHORT_BUFFER_ERANGE EXACT_FIT_OK"),
    "fcntl": ("raw-differential", "pass", "SETFL_IGNORES_OUTSIDE_MASK SETFL_NONBLOCK_ROUNDTRIP SETFL_APPEND_ROUNDTRIP SETFL_ODIRECT_REGULAR SETFL_NOATIME_OWNER SETFL_NOATIME_NONOWNER_EPERM"),
    "syslog": ("raw-differential", "pass", "NULL_BUF_EINVAL NEGATIVE_LEN_EINVAL ZERO_LEN_NOOP BAD_PTR_EFAULT UNKNOWN_ACTION_EINVAL CONSOLE_LEVEL_EINVAL SIZE_BUFFER_POSITIVE"),
    "reboot": ("raw-differential", "pass", "MAGIC1_EINVAL MAGIC2_EINVAL RESTART2_NULL_EFAULT RESTART2_BAD_PTR_EFAULT UNKNOWN_CMD_EINVAL CAPABILITY_BEFORE_MAGIC_EPERM"),
    "ioctl": ("raw-differential", "pass", "FIGETBSZ_PSEUDO_EINVAL FIGETBSZ_FILESYSTEM_BLOCK_SIZE PSEUDO_UNSUPPORTED_REFUSALS FSUUID_AND_SYSSFSPATH_ENOTTY FREEZE_AND_THAW_EPERM_UNPRIVILEGED"),
    "mount": ("raw-differential", "pass", "MS_NOUSER_EINVAL SUPERBLOCK_FLAGS_ACCEPTED MAGIC_MASK_STRIPPED"),
    "umount2": ("raw-differential", "pass", "FLAGS_BEFORE_PATH_EINVAL NOFOLLOW_IS_VALID EXPIRE_ROOT_EINVAL EXPIRE_COMBINATION_EINVAL DETACH_NAMESPACE_ROOT_EINVAL VALID_FLAGS_PATH_VERDICT"),
    "pipe2": ("raw-differential", "pass", "UNKNOWN_FLAG_EINVAL NOTIFICATION_ENOPKG FLAG_SPLIT_AND_CLOEXEC"),
    "syncfs": ("raw-differential", "pass", "PSEUDO_NOOP BAD_FD_EBADF FILESYSTEM_SYNC"),
    "preadv2": ("raw-differential", "pass", "UNKNOWN_FLAG_EOPNOTSUPP APPEND_NOAPPEND_EINVAL HIPRI_ACCEPTED DSYNC_ACCEPTED IOVEC_COPY_BEFORE_FLAGS"),
    "pwritev2": ("raw-differential", "pass", "UNKNOWN_FLAG_EOPNOTSUPP APPEND_NOAPPEND_EINVAL HIPRI_ACCEPTED DSYNC_ACCEPTED IOVEC_COPY_BEFORE_FLAGS"),
    "fallocate-mode": ("raw-differential", "pass", "GEOMETRY_BEFORE_MODE_EINVAL UNKNOWN_MODE_EOPNOTSUPP MODE_BEFORE_ACCESS_EOPNOTSUPP PUNCH_REQUIRES_KEEP_SIZE COLLAPSE_REJECTS_KEEP_SIZE UNSHARE_RANGE_EOPNOTSUPP ZERO_RANGE_ACCEPTED"),
    "tee": ("raw-differential", "pass", "FLAGS_BEFORE_FD_EINVAL ZERO_LEN_BEFORE_FD FD_BEFORE_TYPE_EINVAL SAME_PIPE_EINVAL COPY_RETAINS_SOURCE"),
    "vmsplice": ("raw-differential", "pass", "FLAGS_BEFORE_FD_EINVAL BAD_FD_EBADF NON_PIPE_EBADF EMPTY_IOVEC_ZERO GIFT_AND_READBACK"),
    "readahead-types": ("raw-differential", "pass", "OFFSET_AFTER_TYPE_EINVAL NON_REGULAR_EINVAL REGULAR_ACCEPTED"),
    "pidfd-send-signal": ("raw-differential", "pass", "FLAGS_BEFORE_FD_EINVAL MULTIPLE_SCOPE_EINVAL BAD_FD_EBADF SELF_THREAD_PROBE SIGNO_MISMATCH_EINVAL FD_BEFORE_SIGNO"),
})
CONTRACTS.update({
    "socket_creation_order": ("portable-differential", "pass", "FLAG_MASK_EINVAL TYPE_AT_SOCK_MAX_EINVAL TYPE_MASK_MAX_EINVAL FAMILY_BEFORE_TYPE FAMILY_RANGE_EAFNOSUPPORT INET_TYPE_ZERO_ESOCKTNOSUPPORT INET_RDM_ESOCKTNOSUPPORT INET_PROTOCOL_MISS_EPROTONOSUPPORT INET_PROTOCOL_BELOW_MAX_EPROTONOSUPPORT INET_PROTOCOL_RANGE_EINVAL INET_PROTOCOL_FAR_RANGE_EINVAL INET_RAW_PROTOCOL_RANGE_EINVAL INET_RAW_POLICY"),
    "socket_unix_creation": ("portable-differential", "pass", "UNIX_PROTOCOL_EPROTONOSUPPORT UNIX_TYPE_RDM_ESOCKTNOSUPPORT"),
    "socket_netlink_creation": ("portable-differential", "pass", "NETLINK_TYPE_ESOCKTNOSUPPORT NETLINK_TYPE_BEFORE_PROTOCOL NETLINK_PROTOCOL_RANGE_EPROTONOSUPPORT"),
    "socket_netlink_policy": ("portable-differential", "pass", "USERSOCK_PEER_ALLOWED ROUTE_PEER_POLICY USERSOCK_GROUP_POLICY ROUTE_GROUP_ALLOWED"),
    "socket_address_lengths": ("portable-differential", "pass", "IPV4_BIND_OVERLONG_EINVAL IPV4_BIND_SHORT_EINVAL IPV4_BIND_STORAGE_BOUNDARY IPV6_BIND_RFC2133 IPV6_BIND_SHORT_EINVAL IPV6_BIND_OVERLONG_EINVAL NETLINK_BIND_SHORT_EINVAL NETLINK_BIND_EXACT NETLINK_BIND_LONGER NETLINK_BIND_FAMILY_EINVAL UNIX_BIND_TWO_BYTE_AUTOBINDS UNIX_CONNECT_TWO_BYTE_EINVAL"),
    "socket_sol_socket_table": ("portable-differential", "pass", "GET_SHORT_REPORTS_COPY GET_LINGER_CLAMPED GET_ZERO_LENGTH SET_TYPE_ENOPROTOOPT SET_SNDLOWAT_ENOPROTOOPT SET_PROTOCOL_ENOPROTOOPT SET_SHORT_OPTLEN_EINVAL SET_LINGER_SHORT_EINVAL SET_BAD_LEVEL_ENOPROTOOPT GET_BAD_LEVEL_ENOPROTOOPT"),
    "socket_netlink_option_table": ("portable-differential", "pass", "ADD_ABOVE_NGROUPS_EINVAL ADD_ZERO_EINVAL LISTEN_ALL_NSID_POLICY SET_UNKNOWN_ENOPROTOOPT GET_UNKNOWN_ENOPROTOOPT"),
    "socket_null_operations": ("portable-differential", "pass", "NETLINK_LISTEN_EOPNOTSUPP NETLINK_ACCEPT_EOPNOTSUPP NETLINK_ACCEPT4_EOPNOTSUPP NETLINK_SHUTDOWN_EOPNOTSUPP NETLINK_SHUTDOWN_INVALID_HOW_EOPNOTSUPP NETLINK_SOCKETPAIR_EOPNOTSUPP INET_SOCKETPAIR_EOPNOTSUPP INET_SOCKETPAIR_PROTOCOL_EPROTONOSUPPORT INET_SOCKETPAIR_TYPE_ESOCKTNOSUPPORT SOCKETPAIR_FLAG_MASK_EINVAL"),
})
CONTRACTS.update({
    "lsm-self-attr": ("raw-differential", "pass", "LIST_MODULES_ARGUMENTS GET_SELF_ATTR_ARGUMENT_ORDER GET_SELF_ATTR_UNOWNED_ID_EOPNOTSUPP SET_SELF_ATTR_STRUCTURE SET_SELF_ATTR_UNOWNED_ID_EOPNOTSUPP"),
})
CONTRACTS.update({
    "gettimeofday": ("raw-differential", "pass", "READ_BRACKETS_CLOCK_REALTIME NULL_ARGUMENTS_ACCEPTED TIMEZONE_ROUND_TRIP TIMEZONE_EFAULT"),
    "settimeofday": ("raw-differential", "pass", "SUBSECOND_EINVAL_BEFORE_TIMEZONE TIMEZONE_EFAULT_BEFORE_SECONDS TIMEZONE_RANGE_EINVAL_PRESERVES SET_TO_TIME_BOUND_EINVAL BEFORE_MONOTONIC_EINVAL"),
    "settimeofday-unprivileged": ("raw-differential", "pass", "BOUND_BEFORE_CAPABILITY CAPABILITY_BEFORE_TIMEZONE_RANGE"),
    "clock_settime": ("raw-differential", "pass", "CLOCK_CLASSIFICATION_EINVAL COPY_IN_EFAULT SET_TO_TIME_BOUND_EINVAL BEFORE_MONOTONIC_EINVAL"),
    "clock_getres": ("raw-differential", "pass", "SCHEDULER_CLOCKS_ARE_NANOSECOND ACCOUNTING_CLOCKS_EXCEED_NANOSECOND NULL_AND_UNKNOWN_IDS"),
    "clock_nanosleep": ("raw-differential", "pass", "UNSUPPORTED_CLOCKS_EOPNOTSUPP_BEFORE_COPY UNKNOWN_CLOCK_EINVAL SUPPORTED_CLOCK_COPY_BEFORE_VALIDATION ENCODED_SELF_THREAD_IS_EINVAL PROCESS_CPUTIME_ZERO_INTERVAL"),
    "adjtimex": ("raw-differential", "pass", "BARE_READ_INITIAL_STATE UNKNOWN_MODE_BITS_ECHOED ADJ_ADJTIME_REQUIRES_SINGLESHOT SINGLESHOT_RETURNS_PREVIOUS SS_READ_REPORTS_RESIDUAL STATUS_READONLY_BITS_DROPPED NANO_RESOLUTION_RENDER MICRO_RESOLUTION_CLAMPS MICRO_WINS_OVER_NANO TAI_OFFSET_READBACK SETOFFSET_DISCONTINUITY SETOFFSET_VALIDATION"),
    "clock_adjtime": ("raw-differential", "pass", "REALTIME_SHARES_ADJTIMEX_CORE CLOCKS_WITHOUT_CLOCK_ADJ_ARE_EOPNOTSUPP UNKNOWN_CLOCK_IS_EINVAL COPY_IN_PRECEDES_CLASSIFICATION"),
    "timer_create": ("raw-differential", "pass", "ALARM_COPYOUT_BEFORE_ADMISSION CLOCK_CLASSIFICATION_ERRNOS SIGEV_THREAD_ACCEPTED ALARM_CLOCK_OUTCOME_DOCUMENTED"),
    "timer_getoverrun": ("raw-differential", "pass", "ZERO_BEFORE_FIRST_SIGNAL INVALID_ID_EINVAL"),
    "wait4": ("raw-differential", "pass", "wait4-int-min-esrch wait4-unknown-option wait4-wnowait-unknown wait4-wnohang-flags-zero wait4-wclone-excludes-sigchld wait4-wuntraced-reports-stop wait4-stop-consumed-once wait4-wcontinued-reports-continue wait4-continue-consumed-once wait4-wnohang-zero wait4-reaped-child-echild"),
    "waitid": ("raw-differential", "pass", "waitid-unknown-option waitid-no-event-bit waitid-nowait-alone waitid-p-pid-zero waitid-p-pid-negative waitid-p-pgid-negative waitid-p-pidfd-negative waitid-bad-idtype waitid-p-all-ignores-upid waitid-wnohang-zeroes-info waitid-nowait-preserves-stop waitid-consume-after-nowait waitid-consumed-once waitid-exited-status waitid-reaped-child-echild"),
    "sched_rr_get_interval": ("raw-differential", "pass", "rr-negative-pid rr-self-succeeds rr-null-interval"),
    "sched_attr": ("raw-differential", "pass", "setattr-size-zero-means-ver0 setattr-size-47-e2big setattr-size-4097-e2big setattr-err-size-writes-back setattr-negative-pid setattr-nonzero-flags setattr-null-attr setattr-reclaim-on-other setattr-overrun-on-other setattr-restore setattr-unknown-flag getattr-size-47-einval getattr-size-0-einval getattr-size-4097-einval getattr-size-page-accepted getattr-size-4097-einval getattr-null-attr getattr-negative-pid getattr-unknown-flags getattr-dl-dynamic-on-other getattr-ver0-fields"),
    "mempolicy": ("raw-differential", "pass", "mbind-unaligned-start mbind-unknown-flag mbind-bad-mode-before-mask set_mempolicy-bad-mode set_mempolicy-balancing-default set_mempolicy-balancing-preferred set_mempolicy-balancing-interleave set_mempolicy-static-and-relative set_mempolicy-balancing-bind-reads-mask set_mempolicy-balancing-preferred-many-reads-mask set_mempolicy-static-bind-reads-mask set_mempolicy-relative-bind-reads-mask set_mempolicy-maxnode-too-long set_mempolicy-maxnode-at-bound-faults set_mempolicy-mask-ptr-fault set_mempolicy-bind-mask-ptr-fault set_mempolicy-prefered-mask-ptr-fault set_mempolicy-default-accepts-empty-mask set_mempolicy-default-accepts-absent-mask set_mempolicy-default-rejects-nonempty-mask set_mempolicy-bind-rejects-empty-mask set_mempolicy-bind-rejects-absent-mask set_mempolicy-preferred-empty-is-local set_mempolicy-restore-default get_mempolicy-mems-allowed-skips-addr get_mempolicy-addr-without-flag get_mempolicy-unknown-flags get_mempolicy-f-node-without-addr get_mempolicy-tiny-maxnode"),
    "open-tree": ("raw-differential", "pass", "CLONE_CLOEXEC_IDENTITY FLAG_BITS_BEFORE_PATH RECURSIVE_REQUIRES_CLONE"),
    "fsconfig": ("raw-differential", "pass", "SHAPE_AND_COPY_ORDER UNKNOWN_COMMAND_EOPNOTSUPP CONTEXT_FD_EINVAL"),
    "fsmount": ("raw-differential", "pass", "SCALARS_BEFORE_DESCRIPTOR UNCREATED_CONTEXT_EINVAL TMPFS_CLONE_CLOEXEC"),
    "fspick": ("raw-differential", "pass", "FLAGS_BEFORE_PATH MOUNT_ROOT_ONLY CLOEXEC_CONTEXT_FD"),
    "statmount": ("raw-differential", "pass", "FLAGS_BEFORE_REQUEST BY_FD_REQUEST_EXCLUSIVE BY_FD_EBADF STRING_REQ_EOVERFLOW BY_FD_PREFIX_MASK"),
    "quotactl": ("raw-differential", "pass", "TYPE_BEFORE_PATH BLOCK_DEVICE_REQUIRED NULL_SPECIAL_ERRNO SYNC_ALL_WITHOUT_SPECIAL"),
    "quotactl-fd": ("raw-differential", "pass", "FD_BEFORE_TYPE NON_PATH_FD_ENOSYS QUOTAON_DEFERRED_EINVAL PROVIDER_STATE_ERRNO"),
    "pivot-root": ("raw-differential", "pass", "PATH_BEFORE_CAPABILITY LOOKUP_DIRECTORY_NEW_ROOT LOOKUP_DIRECTORY_PUT_OLD"),
    "fanotify-init": ("raw-differential", "pass", "UNKNOWN_FLAG_EINVAL CLASS_CONFLICT_EINVAL ACCESS_MODE_EINVAL GROUP_FD_CLOEXEC"),
    "fanotify-mark": ("raw-differential", "pass", "SCALARS_BEFORE_DESCRIPTOR DESCRIPTOR_BEFORE_PATH EMPTY_AND_OVERFLOW_MASK SCOPE_RULES_BEFORE_PATH ONLYDIR_TARGET_TYPE FLUSH_IGNORES_PATH"),
    "keyring-random": ("portable-differential", "pass", "GETRANDOM_VALIDATION GETRANDOM_INSECURE_DELIVERY ADD_KEY_VALIDATION REQUEST_KEY_CALLOUT KEYCTL_READ_RULES KEYCTL_VALIDATION"),
    "landlock-create": ("raw-differential", "pass", "ABI_VERSION_10 QUERY_ARGUMENT_ORDER RULESET_SIZE_ORDER ZERO_FILLED_EXTENSION EMPTY_HANDLED_ENOMSG ABI10_HANDLED_MASKS QUIET_MASK_SUBSET"),
    "landlock-add-rule": ("raw-differential", "pass", "RULE_FLAG_VALIDATION EMPTY_ACCESS_ENOMSG UNHANDLED_AND_QUIET_REJECTS DESCRIPTOR_ERROR_ORDER NON_DIRECTORY_ACCESS_FILE RESOLVE_UNIX_RULE_ACCEPTED NET_PORT_UDP_RIGHTS QUIET_RULE_ACCEPTED RULESET_ISOLATION"),
    "landlock-restrict-self": ("raw-differential", "pass", "NO_RULESET_MATRIX RULESET_DESCRIPTOR_ERRORS TSYNC_SYNCHRONIZES_SIBLINGS TSYNC_PROPAGATES_NO_NEW_PRIVS NO_TSYNC_LEAVES_SIBLING"),
    "landlock-unix-resolve": ("raw-differential", "pass", "OUTSIDE_DOMAIN_EACCES ALLOW_RULE_PERMITS SAME_DOMAIN_PERMITS"),
    "landlock-net-port": ("raw-differential", "pass", "BIND_UDP_PORT_RULES BIND_PORT_ZERO_IS_A_RULE_TARGET UDP_AUTOBIND_REQUIRES_PORT_ZERO CONNECT_SEND_UDP_PORT_RULES"),
})
CONTRACTS.update({
    "futex-abi-opcode": ("portable-differential", "pass", "WAKE_REALTIME LOCK_PI_REALTIME WAIT_BITSET_REALTIME LOCK_PI2_REALTIME WAKE_HIGH_BIT WAKE_BIT11 WAIT_ROBUST_UNLOCK LOCK_PI_ROBUST_UNLOCK FD MISALIGNED WAKE_BITSET_ZERO WAKE_ZERO_NO_WAITERS"),
    "futex-abi-wake-zero": ("portable-differential", "pass", "WAKE_ZERO_LIMIT"),
    "futex-abi-requeue": ("portable-differential", "pass", "WOKEN REQUEUED DRAINED EAGAIN"),
    "futex-abi-pi-word": ("portable-differential", "pass", "LOCKED UNLOCK_RC_ZERO UNLOCK_WORD_ZERO EPERM TRYLOCK"),
    "futex-abi-pi-timeout": ("portable-differential", "pass", "ETIMEDOUT LOCKPI2"),
    "futex-abi-requeue-pi": ("portable-differential", "pass", "REQUEUED WAITER_RC TARGET_WORD UNLOCKED EINVAL_SELF EINVAL_WAKE2"),
    "futex-abi-futex2-flags": ("portable-differential", "pass", "NUMA_OK NUMA_EINVAL MPOL_OK RESERVED_EINVAL SIZE_EINVAL ALIGN_EINVAL MASK0_EINVAL"),
})
CONTRACTS.update({
    "sysadmin-abi.ptrace-requests": ("raw-differential", "pass", "PTRACE_SETOPTIONS_UNKNOWN_BITS_EINVAL PTRACE_OLDSETOPTIONS_ACCEPTED PTRACE_SETOPTIONS_EXITKILL_ACCEPTED PTRACE_EXITKILL_ACCEPTED PTRACE_SUSPEND_SECCOMP_ADMITTED PTRACE_SEIZE_SUSPEND_SECCOMP_GATE PTRACE_UNKNOWN_REQUEST_EIO PTRACE_RESUME_BAD_SIGNAL_EIO PTRACE_GETREGSET_UNKNOWN_TYPE_EINVAL PTRACE_GET_SYSCALL_INFO_SIZE PTRACE_GET_SYSCALL_INFO_OP PTRACE_GET_SYSCALL_INFO_ARCH PTRACE_GET_SYSCALL_INFO_RESERVED PTRACE_GET_SYSCALL_INFO_FLAGS PTRACE_GET_SYSCALL_INFO_SHORT_BUFFER_SIZE PTRACE_GET_SYSCALL_INFO_SHORT_BUFFER_OP PTRACE_DETACH_ACCEPTED PTRACE_TRACEE_EXIT_STATUS"),
    "sysadmin-abi.unshare-flags": ("raw-differential", "pass", "UNSHARE_ZERO_NOOP UNSHARE_THREAD_NOOP UNSHARE_SIGHAND_NOOP UNSHARE_VM_NOOP UNSHARE_THREAD_FS_NOOP UNSHARE_FILES_OK UNSHARE_PIDFD_EINVAL UNSHARE_SETTLS_EINVAL UNSHARE_IO_EINVAL UNSHARE_CLEAR_SIGHAND_EINVAL"),
    "sysadmin-abi.swap-flags": ("raw-differential", "pass", "SWAPON_BAD_FLAGS_EINVAL SWAPON_BAD_FLAGS_ABSENT_PATH_EINVAL SWAPON_EMPTY_PATH_ENOENT SWAPOFF_EMPTY_PATH_ENOENT"),
    "sysadmin-abi.module-image": ("raw-differential", "pass", "INIT_MODULE_ZERO_LEN_ENOEXEC INIT_MODULE_SHORT_IMAGE_ENOEXEC INIT_MODULE_HEADER_SIZE_NULL_EFAULT INIT_MODULE_BAD_IMAGE_PRECEDES_BAD_PARAMS DELETE_MODULE_EMPTY_NAME_ENOENT DELETE_MODULE_UNKNOWN_NAME_ENOENT DELETE_MODULE_STRAY_FLAGS_STILL_ENOENT"),
    "sysvipc-ids": ("reuse-progression", "pass", "IDENTIFIER_NEVER_REUSED_IMMEDIATELY RETIRED_IDENTIFIER_STAYS_INVALID NO_IDENTIFIER_REPEAT_WITHIN_CYCLE"),
    "sysvipc-stat": ("index-resolution", "pass", "STAT_RETURNS_FULL_IDENTIFIER STAT_MASKS_INDEX_BITS IPC_STAT_RETURNS_ZERO"),
    "sysvipc-info": ("max-index", "pass", "INFO_AND_FAMILY_INFO_AGREE MAX_INDEX_COVERS_LIVE_OBJECT"),
    "sysvipc-sem": ("flags", "pass", "UNKNOWN_FLAG_BITS_IGNORED WAIT_ZERO_WITH_UNDO_SUCCEEDS WAIT_ZERO_WITH_UNDO_EAGAIN"),
    "sysvipc-sem-undo": ("undo-range", "pass", "UNDO_RANGE_REPORTS_ERANGE ERANGE_LEAVES_VALUE_UNCHANGED"),
    "sysvipc-control": ("ipc64", "pass", "IPC64_COMMAND_EINVAL PLAIN_COMMAND_STILL_WORKS"),
    "sysvipc-shm-lock": ("lock-memlock", "pass", "SHM_LOCK_ZERO_MEMLOCK_EPERM"),
    "sysvipc-shm-hugetlb": ("hugetlb-existing-key", "pass", "EXISTING_KEY_IGNORES_HUGETLB"),
    "sysvipc-shm-dest": ("dest-stat", "pass", "DEST_SEGMENT_STILL_STATABLE DEST_SEGMENT_RETURNS_FULL_ID DESTROYED_SEGMENT_EINVAL"),
    "sysvipc-errno": ("order", "pass", "SEMOP_EFBIG_BEFORE_EACCES MSGSND_FAULTS_BEFORE_VALIDATION MSGSND_SIZE_AND_TYPE_BEFORE_ID TABLE_COMMANDS_REJECT_NEGATIVE_ID SEMCTL_VALUE_AND_SEMNUM_ORDER SEMTIMEDOP_COUNT_AND_TIMEOUT_ORDER"),
    "sysvipc-msg": ("exclusive-create-order", "pass", "EXCLUSIVE_CREATE_BEFORE_PERMISSION"),
    "mq_open": ("raw-differential", "pass", "DEFAULT_ATTRIBUTES HARD_LIMIT_CAPABILITY RLIMIT_CHARGE_BOUNDARY OPEN_EXISTING_PRECEDENCE CREATE_ACCMODE_THREE NAME_SYNTAX NAME_LENGTH_BOUNDARY OPEN_FD_LIFETIME"),
    "mq_unlink": ("raw-differential", "pass", "UNLINK_NAME_SYNTAX STICKY_DIRECTORY_EPERM OWNER_AND_REUSE OWNER_AUTHORITY"),
    "mq_timedsend": ("raw-differential", "pass", "ARGUMENT_PRECEDENCE FULL_QUEUE_EAGAIN PRIORITY_ORDER BLOCKED_SENDER_HANDOFF"),
    "mq_timedreceive": ("raw-differential", "pass", "EMSGSIZE_BEFORE_EAGAIN PRIORITY_ORDER DEQUEUE_BEFORE_COPYOUT"),
    "mq_notify": ("raw-differential", "pass", "ONE_SHOT_EMPTY_EDGE EMPTY_EDGE_REARM ARGUMENT_VALIDATION PIPELINED_RECEIVE_SKIPS_NOTIFY"),
})
CONTRACTS.update({
    "console_integrity": ("raw-differential", "pass", "INTERLEAVED_WRITERS"),
})
PROGRAM_CASES = {
    "tty-job-control": ("tty-job-control",),
    "tty-termios": ("tty-termios",),
    "unix-write-credentials": ("unix-write-credentials",),
    "scheduler-basic": ('sched_getaffinity', 'sched_setaffinity', 'getcpu', 'sched_setparam', 'sched_setscheduler', 'sched_get_priority_max', 'sched_get_priority_min'),
    "fs-boundary": ("flock", "utimensat", "fallocate", "readahead", "inotify_add_watch", "signalfd4", "timerfd_settime"),
    "eventfd": ("eventfd",), "creat": ("creat",), "time": ("time",),
    "umask": ("umask",), "native-ni": ("native-ni",),
    "fsattrs": ("setxattrat", "getxattrat", "listxattrat", "removexattrat", "file-getattr", "file-setattr", "open-tree-attr"),
    "mm-contracts": ("mprotect", "munmap", "mincore", "process-vm-readv", "process-vm-writev", "mseal", "mlock", "mlock2", "munlock", "process-madvise", "mlockall"),
    "network-basic": ("network_bind", "network_connect", "network_getpeername", "network_sendto"),
    "signal-boundary": ("rt_sigaction", "sigaltstack", "rt_tgsigqueueinfo", "restart_syscall"),
    "stat-access": ("access", "faccessat", "faccessat2", "newfstatat", "statx"),
    "socket-msg": ("socket_msg.send_flags", "socket_msg.tcp_more", "socket_msg.compat_flag", "socket_msg.waitall_stream", "socket_msg.waitall_tcp", "socket_msg.waitall_datagram", "socket_msg.recvmmsg_deadline", "socket_msg.recvmmsg_waitforone"),
    "task-control": ("prctl-name", "prctl-timing", "prctl-auxv", "prctl-timer-restore-ids", "prctl-cfi", "arch_prctl", "iopl", "capset", "move_pages", "modify_ldt", "setpgid", "clone3"),
    "fs-abi": ("getcwd", "fcntl", "syslog", "reboot", "ioctl", "mount", "umount2", "pipe2", "syncfs", "preadv2", "pwritev2", "fallocate-mode", "tee", "vmsplice", "readahead-types", "pidfd-send-signal"),
    "epoll-membarrier": ("epoll-membarrier",),
    "socket-provider": ("socket_creation_order", "socket_unix_creation", "socket_netlink_creation", "socket_netlink_policy", "socket_address_lengths", "socket_sol_socket_table", "socket_netlink_option_table", "socket_null_operations"),
    "lsm-abi": ("lsm-self-attr",),
    "time-abi": ("gettimeofday", "settimeofday", "settimeofday-unprivileged", "clock_settime", "clock_getres", "clock_nanosleep", "adjtimex", "clock_adjtime", "timer_create", "timer_getoverrun"),
    "wait-abi": ("wait4", "waitid", "sched_rr_get_interval", "sched_attr", "mempolicy"),
    "mount-api": ("open-tree", "fsconfig", "fsmount", "fspick", "statmount", "quotactl", "quotactl-fd", "pivot-root", "fanotify-init", "fanotify-mark"),
    "keyring-random": ("keyring-random",),
    "landlock-abi": ("landlock-create", "landlock-add-rule", "landlock-restrict-self", "landlock-unix-resolve", "landlock-net-port"),
    "futex-abi": ("futex-abi-opcode", "futex-abi-wake-zero", "futex-abi-requeue", "futex-abi-pi-word", "futex-abi-pi-timeout", "futex-abi-requeue-pi", "futex-abi-futex2-flags"),
    "sysadmin-abi": ("sysadmin-abi.ptrace-requests", "sysadmin-abi.unshare-flags", "sysadmin-abi.swap-flags", "sysadmin-abi.module-image"),
    "sysv-ipc": ("sysvipc-ids", "sysvipc-stat", "sysvipc-info", "sysvipc-sem", "sysvipc-sem-undo", "sysvipc-control", "sysvipc-shm-lock", "sysvipc-shm-hugetlb", "sysvipc-shm-dest", "sysvipc-errno", "sysvipc-msg"),
    "posix-mqueue": ("mq_open", "mq_unlink", "mq_timedsend", "mq_timedreceive", "mq_notify"),
    "console-integrity": ("console_integrity",),
}
# The registry is static: the gate reads it to decide whether a claimed
# syscall names a program this runner really executes.
PROGRAMS = tuple(PROGRAM_CASES)


def selected_programs() -> tuple[str, ...]:
    """The guest programs the current run exercises.

    ``THEKERNEL_ABI_PROGRAMS`` narrows the set to a comma-separated list so a
    single failing program can be reproduced without paying for a boot that
    runs every other one first.  The comparison stays meaningful because both
    guests run the same narrowed list and every assertion of every selected
    program is still required.  Unset means every registered program, which is
    what the gate always sees because it does not run a guest.
    """
    requested = os.environ.get("THEKERNEL_ABI_PROGRAMS")
    if requested is None:
        return PROGRAMS
    selected = tuple(name.strip() for name in requested.split(",") if name.strip())
    unknown = [name for name in selected if name not in PROGRAM_CASES]
    if unknown or not selected:
        raise RunnerError(
            "THEKERNEL_ABI_PROGRAMS must name registered programs; "
            f"unknown={unknown or requested!r}"
        )
    return selected
PROGRAM_SUCCESS = {
    "tty-job-control": "THEKERNEL_TTY_JOB_CONTROL_OK",
    "tty-termios": "THEKERNEL_TTY_TERMIOS_OK",
    "unix-write-credentials": "THEKERNEL_UNIX_WRITE_CREDENTIALS_OK",
    "scheduler-basic": "THEKERNEL_SCHEDULER_BASIC_DIFFERENTIAL_OK",
    "fs-boundary": "THEKERNEL_FS_BOUNDARY_PASS",
    "stat-access": "THEKERNEL_STAT_ACCESS_OK",
    "eventfd": "THEKERNEL_EVENTFD_OK", "creat": "THEKERNEL_CREAT_OK",
    "time": "THEKERNEL_TIME_OK", "umask": "THEKERNEL_UMASK_OK",
    "native-ni": "THEKERNEL_NATIVE_NI_OK", "fsattrs": "THEKERNEL_FSATTRS_OK",
    "mm-contracts": "THEKERNEL_MM_CONTRACTS_OK", "network-basic": "THEKERNEL_NETWORK_BASIC_PASS",
    "signal-boundary": "THEKERNEL_SIGNAL_BOUNDARY_PASS",
    "socket-msg": "THEKERNEL_SOCKET_MSG_OK",
    "task-control": "THEKERNEL_TASK_CONTROL_OK",
    "fs-abi": "THEKERNEL_FS_ABI_OK",
    "epoll-membarrier": "THEKERNEL_EPOLL_MEMBARRIER_OK",
    "socket-provider": "THEKERNEL_SOCKET_PROVIDER_PASS",
    "lsm-abi": "THEKERNEL_LSM_ABI_OK",
    "time-abi": "THEKERNEL_TIME_ABI_OK",
    "wait-abi": "THEKERNEL_WAIT_ABI_DIFFERENTIAL_OK",
    "mount-api": "THEKERNEL_MOUNT_API_OK",
    "keyring-random": "THEKERNEL_KEYRING_RANDOM_OK",
    "landlock-abi": "THEKERNEL_LANDLOCK_ABI_OK",
    "futex-abi": "THEKERNEL_FUTEX_ABI_DIFFERENTIAL_OK",
    "sysadmin-abi": "THEKERNEL_SYSADMIN_ABI_OK",
    "sysv-ipc": "THEKERNEL_SYSVIPC_OK",
    "posix-mqueue": "THEKERNEL_POSIX_MQUEUE_OK",
    "console-integrity": "THEKERNEL_CONSOLE_INTEGRITY_OK",
}
PROGRAM_COMPLETIONS = tuple(PROGRAM_SUCCESS.values())

# A test file existing is insufficient: a claimed syscall must name a case
# that this runner actually requires from the registered guest program.
SYSCALL_CASES = {
    204: ("scheduler-basic", "sched_getaffinity"),
    203: ("scheduler-basic", "sched_setaffinity"),
    309: ("scheduler-basic", "getcpu"),
    142: ("scheduler-basic", "sched_setparam"),
    144: ("scheduler-basic", "sched_setscheduler"),
    146: ("scheduler-basic", "sched_get_priority_max"),
    147: ("scheduler-basic", "sched_get_priority_min"),

    254: ("fs-boundary", "inotify_add_watch"), 289: ("fs-boundary", "signalfd4"),
    286: ("fs-boundary", "timerfd_settime"), 149: ("mm-contracts", "mlock"),
    325: ("mm-contracts", "mlock2"), 150: ("mm-contracts", "munlock"),
    440: ("mm-contracts", "process-madvise"), 151: ("mm-contracts", "mlockall"),
    73: ("fs-boundary", "flock"), 280: ("fs-boundary", "utimensat"),
    285: ("fs-boundary", "fallocate"), 187: ("fs-boundary", "readahead"),
    21: ("stat-access", "access"), 269: ("stat-access", "faccessat"),
    439: ("stat-access", "faccessat2"), 262: ("stat-access", "newfstatat"),
    332: ("stat-access", "statx"),
    297: ("signal-boundary", "rt_tgsigqueueinfo"),
    13: ("signal-boundary", "rt_sigaction"), 131: ("signal-boundary", "sigaltstack"),
    85: ("creat", "creat"), 95: ("umask", "umask"), 201: ("time", "time"),
    284: ("eventfd", "eventfd"), 290: ("eventfd", "eventfd"),
    463: ("fsattrs", "setxattrat"), 464: ("fsattrs", "getxattrat"),
    465: ("fsattrs", "listxattrat"), 466: ("fsattrs", "removexattrat"),
    467: ("fsattrs", "open-tree-attr"), 468: ("fsattrs", "file-getattr"),
    469: ("fsattrs", "file-setattr"),
    10: ("mm-contracts", "mprotect"), 11: ("mm-contracts", "munmap"),
    27: ("mm-contracts", "mincore"), 310: ("mm-contracts", "process-vm-readv"),
    311: ("mm-contracts", "process-vm-writev"), 462: ("mm-contracts", "mseal"),
    # Message-transfer flags, byte-stream MSG_WAITALL and recvmmsg deadlines.
    44: ("socket-msg", "socket_msg.send_flags"),
    45: ("socket-msg", "socket_msg.waitall_stream"), 47: ("socket-msg", "socket_msg.waitall_stream"),
    46: ("socket-msg", "socket_msg.send_flags"), 307: ("socket-msg", "socket_msg.send_flags"),
    299: ("socket-msg", "socket_msg.recvmmsg_deadline"),
    # prctl and the task-control syscalls it fronts.
    157: ("task-control", "prctl-name"), 158: ("task-control", "arch_prctl"),
    109: ("task-control", "setpgid"), 126: ("task-control", "capset"),
    154: ("task-control", "modify_ldt"), 172: ("task-control", "iopl"),
    279: ("task-control", "move_pages"), 435: ("task-control", "clone3"),
    233: ("epoll-membarrier", "epoll-membarrier"), 324: ("epoll-membarrier", "epoll-membarrier"),
    219: ("epoll-membarrier", "epoll-membarrier"),
    # Filesystem and descriptor boundary rules.
    16: ("fs-abi", "ioctl"), 72: ("fs-abi", "fcntl"), 79: ("fs-abi", "getcwd"),
    103: ("fs-abi", "syslog"), 165: ("fs-abi", "mount"), 166: ("fs-abi", "umount2"),
    169: ("fs-abi", "reboot"), 276: ("fs-abi", "tee"), 278: ("fs-abi", "vmsplice"),
    293: ("fs-abi", "pipe2"), 306: ("fs-abi", "syncfs"), 424: ("fs-abi", "pidfd-send-signal"),
    327: ("fs-abi", "preadv2"), 328: ("fs-abi", "pwritev2"),
    # Socket provider creation, address length and option-table rules.
    41: ("socket-provider", "socket_creation_order"),
    53: ("socket-provider", "socket_null_operations"),
    49: ("socket-provider", "socket_address_lengths"),
    54: ("socket-provider", "socket_sol_socket_table"),
    55: ("socket-provider", "socket_netlink_option_table"),
    42: ("socket-provider", "socket_netlink_policy"),
    51: ("socket-provider", "socket_netlink_policy"),
    52: ("socket-provider", "socket_netlink_policy"),
    43: ("socket-provider", "socket_null_operations"),
    288: ("socket-provider", "socket_null_operations"),
    48: ("socket-provider", "socket_null_operations"),
    50: ("socket-provider", "socket_null_operations"),
    # LSM attribute syscalls.
    461: ("lsm-abi", "lsm-self-attr"), 459: ("lsm-abi", "lsm-self-attr"),
    460: ("lsm-abi", "lsm-self-attr"),
    # Clocks, timers and the timex state machine.
    96: ("time-abi", "gettimeofday"), 164: ("time-abi", "settimeofday"),
    159: ("time-abi", "adjtimex"), 305: ("time-abi", "clock_adjtime"),
    222: ("time-abi", "timer_create"), 225: ("time-abi", "timer_getoverrun"),
    227: ("time-abi", "clock_settime"), 229: ("time-abi", "clock_getres"),
    230: ("time-abi", "clock_nanosleep"), 228: ("time-abi", "gettimeofday"),
    # Child reaping, scheduler attributes and the memory policy ABI.
    61: ("wait-abi", "wait4"), 247: ("wait-abi", "waitid"),
    148: ("wait-abi", "sched_rr_get_interval"),
    314: ("wait-abi", "sched_attr"), 315: ("wait-abi", "sched_attr"),
    237: ("wait-abi", "mempolicy"), 238: ("wait-abi", "mempolicy"),
    239: ("wait-abi", "mempolicy"),
    # Mount API, quota and fanotify validation order.
    428: ("mount-api", "open-tree"), 431: ("mount-api", "fsconfig"),
    432: ("mount-api", "fsmount"), 433: ("mount-api", "fspick"),
    457: ("mount-api", "statmount"), 179: ("mount-api", "quotactl"),
    443: ("mount-api", "quotactl-fd"), 155: ("mount-api", "pivot-root"),
    300: ("mount-api", "fanotify-init"), 301: ("mount-api", "fanotify-mark"),
    # Randomness, keyrings and the Landlock ABI10 surface.
    318: ("keyring-random", "keyring-random"), 248: ("keyring-random", "keyring-random"),
    249: ("keyring-random", "keyring-random"), 250: ("keyring-random", "keyring-random"),
    444: ("landlock-abi", "landlock-create"), 445: ("landlock-abi", "landlock-add-rule"),
    446: ("landlock-abi", "landlock-restrict-self"),
    # Priority inheritance, requeue and the futex2 NUMA extension.
    202: ("futex-abi", "futex-abi-opcode"),
    454: ("futex-abi", "futex-abi-futex2-flags"), 455: ("futex-abi", "futex-abi-futex2-flags"),
    # System administration, module images and the ptrace request table.
    101: ("sysadmin-abi", "sysadmin-abi.ptrace-requests"),
    272: ("sysadmin-abi", "sysadmin-abi.unshare-flags"),
    167: ("sysadmin-abi", "sysadmin-abi.swap-flags"),
    168: ("sysadmin-abi", "sysadmin-abi.swap-flags"),
    175: ("sysadmin-abi", "sysadmin-abi.module-image"),
    313: ("sysadmin-abi", "sysadmin-abi.module-image"),
    176: ("sysadmin-abi", "sysadmin-abi.module-image"),
    # System V IPC identity, index semantics and errno order.
    68: ("sysv-ipc", "sysvipc-ids"), 71: ("sysv-ipc", "sysvipc-ids"),
    64: ("sysv-ipc", "sysvipc-sem"), 65: ("sysv-ipc", "sysvipc-sem"),
    66: ("sysv-ipc", "sysvipc-sem"), 220: ("sysv-ipc", "sysvipc-errno"),
    69: ("sysv-ipc", "sysvipc-errno"), 70: ("sysv-ipc", "sysvipc-errno"),
    29: ("sysv-ipc", "sysvipc-shm-lock"), 30: ("sysv-ipc", "sysvipc-shm-dest"),
    31: ("sysv-ipc", "sysvipc-shm-dest"),
    # POSIX message queues.
    240: ("posix-mqueue", "mq_open"), 241: ("posix-mqueue", "mq_unlink"),
    242: ("posix-mqueue", "mq_timedsend"), 243: ("posix-mqueue", "mq_timedreceive"),
    244: ("posix-mqueue", "mq_notify"),
}


@dataclass(frozen=True)
class AbiConfig:
    targets: tuple[BenchmarkTarget, ...]
    rootfs: Path
    workdir: Path
    cpus: int = 4
    memory: str = "4G"
    timeout: float = 1800.0


def expected_records(programs: tuple[str, ...] | None = None) -> list[str]:
    wanted = {case for name in (PROGRAMS if programs is None else programs) for case in PROGRAM_CASES[name]}
    records = []
    for name, (suffix, outcome, assertions) in CONTRACTS.items():
        if name not in wanted:
            continue
        case = f"{name}.{suffix}"
        records.append(f"THEKERNEL_ABI_CASE {case}")
        records.extend(f"THEKERNEL_ABI_ASSERT {case} {assertion} {outcome}" for assertion in assertions.split())
        records.append(f"THEKERNEL_ABI_RESULT {case} {outcome}")
    return records


def parse_abi_log(path: Path, *, linux: bool = False, programs: tuple[str, ...] | None = None) -> Counter:
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    if programs is None:
        programs = PROGRAMS
    completions = tuple(PROGRAM_SUCCESS[name] for name in programs)
    if lines.count(COMPLETE_MARKER) != 1:
        raise RunnerError(f"ABI guest did not complete exactly once: {path}")
    if any(lines.count(marker) != 1 for marker in completions):
        raise RunnerError(f"ABI program completion is missing or duplicated: {path}")
    if any(lines.index(marker) > lines.index(COMPLETE_MARKER) for marker in completions):
        raise RunnerError(f"ABI aggregate completion precedes a program completion: {path}")
    if re.search(r"^THEKERNEL_\S*(?:FAIL|SKIP)(?:\s|$)", text, re.MULTILINE):
        raise RunnerError(f"ABI guest reported a failure or skip: {path}")
    intervals = [line for line in lines if line.startswith("# THEKERNEL_TEST_")]
    if len(intervals) != 2 * len(programs):
        raise RunnerError(f"ABI watchdog intervals are missing or duplicated: {path}")
    owned_record_count = 0
    for index, name in enumerate(programs, 1):
        begin = rf"# THEKERNEL_TEST_BEGIN {index} abi-{re.escape(name)} timeout_seconds=[1-9][0-9]*"
        end = f"# THEKERNEL_TEST_END {index} abi-{name} result=0"
        if not re.fullmatch(begin, intervals[2 * (index - 1)]) or intervals[2 * index - 1] != end:
            raise RunnerError(f"ABI program lacks a unique successful watchdog interval: {name}: {path}")
        first, last = lines.index(intervals[2 * (index - 1)]), lines.index(end)
        if last >= lines.index(COMPLETE_MARKER):
            raise RunnerError(f"ABI watchdog interval outlives aggregate completion: {path}")
        expected_case_names = {f"{case}.{CONTRACTS[case][0]}" for case in PROGRAM_CASES[name]}
        owned = [line for line in lines[first + 1:last] if line.startswith("THEKERNEL_ABI_")]
        owned_record_count += len(owned)
        if (not owned or any(len(line.split()) < 2 or line.split()[1] not in expected_case_names for line in owned)
                or PROGRAM_SUCCESS[name] not in lines[first + 1:last]):
            raise RunnerError(f"ABI program records do not belong to its watchdog interval: {name}: {path}")
    if linux:
        validate_linux_boot(text, path)
    ordered = [line for line in lines if line.startswith("THEKERNEL_ABI_")]
    if owned_record_count != len(ordered) - 1:
        raise RunnerError(f"ABI case records escaped their program watchdog interval: {path}")
    records = Counter(ordered)
    expected = Counter(expected_records(programs) + [COMPLETE_MARKER])
    if records != expected:
        raise RunnerError(f"ABI assertions missing, duplicated or unexpected: {path}; missing={list((expected - records).elements())}; unexpected={list((records - expected).elements())}")
    active = None
    for record in ordered[:-1]:
        fields = record.split()
        if fields[0] == "THEKERNEL_ABI_CASE" and active is None:
            active = fields[1]
        elif fields[0] == "THEKERNEL_ABI_ASSERT" and fields[1] == active:
            pass
        elif fields[0] == "THEKERNEL_ABI_RESULT" and fields[1] == active:
            active = None
        else:
            raise RunnerError(f"ABI record is outside its active case: {path}: {record}")
    if active is not None or ordered[-1] != COMPLETE_MARKER:
        raise RunnerError(f"ABI completion precedes finished cases: {path}")
    return records


def run_abi_differential(config: AbiConfig) -> Path:
    if sorted(target.name for target in config.targets) != ["baseline", "linux"]:
        raise RunnerError("ABI differential requires exactly baseline and linux targets")
    if config.cpus not in (1, 4) or not math.isfinite(config.timeout) or config.timeout <= 0:
        raise RunnerError("ABI differential requires 1/4 vCPUs and a positive timeout")
    validate_storage(config.workdir)
    for path in (config.rootfs, *(p for target in config.targets for p in (target.kernel, target.esp))):
        if not path.is_file() or not path.stat().st_size:
            raise RunnerError(f"ABI input is missing or empty: {path}")
    programs = selected_programs()
    config.workdir.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="abi-", dir=config.workdir))
    base = directory / "rootfs-base.img"
    observations = []
    boot_copies = []
    try:
        targets = []
        for target in config.targets:
            current = directory / target.name
            current.mkdir()
            kernel, esp = current / "kernel", current / "boot.esp"
            boot_copies.extend((kernel, esp))
            shutil.copyfile(target.kernel, kernel)
            shutil.copyfile(target.esp, esp)
            validate = validate_linux_esp_kernel if target.name == "linux" else validate_thekernel_esp_kernel
            validate(kernel, esp)
            targets.append(BenchmarkTarget(target.name, kernel, esp))
        shutil.copyfile(config.rootfs, base)
        for target in targets:
            current = directory / target.name
            rootfs = current / "rootfs.img"
            commands = current / "commands"
            workloads = []
            case_timeout = min(120, max(1, math.ceil(config.timeout)))
            for index, name in enumerate(programs, 1):
                command = f"/opt/thekernel-tests/portable/{name}-differential"
                if name == "unix-write-credentials":
                    command += " --require-id-change"
                workloads.extend((
                    f'echo "# THEKERNEL_TEST_BEGIN {index} abi-{name} timeout_seconds={case_timeout}"',
                    f"{command}; result=$?",
                    '[ "$result" = 0 ] || failed=1',
                    f'echo "# THEKERNEL_TEST_END {index} abi-{name} result=$result"',
                ))
            commands.write_text(
                "failed=0\n" +
                # Linux DEBUG_STACK_USAGE informational printk output can split
                # userspace assertion lines on the serial console. Keep errors
                # visible and retain the already emitted boot version banner.
                ("echo 3 > /proc/sys/kernel/printk || failed=1\n"
                 # The shell init has no network service. UDP error-queue
                 # contracts require the same usable loopback as TheKernel.
                 "ip link set lo up || failed=1\n"
                 # The shell init is not a full init, so nothing mounts
                 # devpts and a Unix98 PTY slave has no /dev/pts/<N> to open;
                 # grantpt() fails with ENOENT before any tty semantics can be
                 # compared. TheKernel's pseudofs always exposes the PTY
                 # namespace, so mount devpts here to keep both guests
                 # equivalent.
                 "mkdir -p /dev/pts && mount -t devpts devpts /dev/pts || failed=1\n"
                 if target.name == "linux" else "") +
                "\n".join(workloads) + "\n" +
                f'[ "$failed" = 0 ] && echo {COMPLETE_MARKER}\n'
                "/bin/busybox poweroff -f\nexit\n", encoding="utf-8")
            try:
                shutil.copyfile(base, rootfs)
                with rootfs.open("r+b") as image:
                    os.fsync(image.fileno())
                result = run(RunConfig(
                    arch="x86_64", kernel=target.kernel, esp=target.esp,
                    rootfs=rootfs, rootfs_transport="drive", rootfs_mode="rw",
                    workdir=current, log_path=current / "console.log", input_path=commands,
                    limits=RunLimits(total_timeout_secs=config.timeout),
                    interaction=Interaction(interactive=True, input_after_marker=SHELL_MARKER,
                                            input_line_after_marker=SHELL_MARKER),
                    memory=config.memory, cpus=config.cpus, accel="kvm", graphics_profile="headless",
                ))
                if (not result.guest_clean_shutdown or result.error_message is not None
                        or result.runner_termination_reason is not None):
                    raise RunnerError(f"ABI guest failed: {target.name}; log={result.log_path}")
                observations.append(parse_abi_log(result.log_path, linux=target.name == "linux", programs=programs))
            finally:
                rootfs.unlink(missing_ok=True)
    finally:
        base.unlink(missing_ok=True)
        for path in boot_copies:
            path.unlink(missing_ok=True)
    if observations[0] != observations[1]:
        raise RunnerError("Linux and TheKernel ABI observations differ")
    return directory
