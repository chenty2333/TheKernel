# Kernel diagnostics and request tracing

The user terminal and kernel diagnostics have separate destinations. Standard
QEMU runs capture COM1 in `console.log` and COM2 in `kernel.log`, both under the
run directory. Interactive input, program output, and test completion markers
belong to COM1. A marker in `kernel.log` cannot make a guest test pass.

CPU boot capability reports and early clock diagnostics go to COM2. The CPU
suite reads them from `kernel.log`; its userspace KTAP results remain in
`console.log`. Panic output uses a bounded emergency COM2 writer that does not
acquire the normal logger's locks. A machine without COM2 still retains normal
kernel logs in memory; emergency output is best effort.

## Kernel logs

Normal `log` macros produce complete bounded records with severity, timestamp,
CPU, task, target, and module. Severity is independent of terminal colors.
Producers format into fixed storage and attempt to append without waiting for a
log lock or a serial device. A dedicated task drains diagnostics when work is
pending. It preserves partial UART writes and backs off when output stalls.
Diagnostic-worker failure retires the output sink rather than stopping the OS.

The default capture level is `info`. To inspect retained logs in the guest:

```sh
dmesg
cat /proc/sys/kernel/log_filter
cat /proc/sys/kernel/log_stats
```

Change capture filtering as a guest task with `CAP_SYSLOG`:

```sh
echo 'info,thekernel_kernel::file::io_uring=debug' > /proc/sys/kernel/log_filter
# Reproduce the operation, then restore the normal filter.
echo info > /proc/sys/kernel/log_filter
```

The first item is the default level (`off`, `error`, `warn`, `info`, `debug`, or
`trace`). Later items are `target_prefix=level`; the longest matching prefix
wins. Each write replaces the whole filter. Up to 16 unique ASCII prefixes of
64 bytes are accepted. An invalid replacement leaves the previous filter intact.
Use the `target` field in a log record to choose a prefix.

The build environment's `AX_LOG` sets the initial filter. Module overrides can
enable debug records at runtime without rebuilding. Syslog console controls
affect the diagnostic sink only; they do not alter the user terminal or erase
retained records.

Storage is deliberately bounded: a 64 KiB retained text ring and a separate
64-record diagnostic queue, with at most 1024 bytes per formatted record.
Contention or reentrancy can drop a record; a full diagnostic queue can drop its
serial copy while retaining the text. `log_stats` reports these losses,
truncation, retention overwrite, and sink availability. Missing output is not
evidence that an event did not happen when the relevant loss counter increased.

Run `/opt/thekernel-tests/bin/thekernel-kernel-bench diagnostics` as guest root
to check filter replacement, permissions (including inherited descriptors),
statistics, and retained debug records while diagnostic output is disabled.
The opt-in check restores the filter and prints `THEKERNEL_LOG_DIAGNOSTICS_OK`
on success.
The system guest suite runs this check and the io_uring trace check below.

## io_uring lifecycle capture

Lifecycle capture is independent of log verbosity and is off by default. It
records committed request transitions using the kernel identity
`ring`, `slot`, and `generation`; `user_data` alone is not a unique identity.
Enable it around a focused reproduction in the guest:

```sh
cd /sys/kernel/tracing/io_uring
echo 0 > enable
echo > trace
echo 1 > enable
# Run the operation being investigated.
echo 0 > enable
cat trace
cat dropped
```

The snapshot includes reservation, submission, issue, accepted completion,
CQ publication start/rollback/commit, provider cancellation selection/results,
and request rollback/discard. A completion accepted by the
kernel is distinct from a CQE successfully published to the ring. CQ head
reclamation is reported as an aggregate ring/head/count observation; it does
not prove that userspace consumed a particular request's result.

This is a global kernel capture. Reading it or changing its controls requires
`CAP_SYS_ADMIN` in the initial user namespace. It lives in an explicitly private
tracefs subtree and is not advertised as a Linux perf tracepoint.

Capture holds 1024 events and stops adding records when full, preserving the
start of the reproduction. Contention and full-buffer drops advance the loss
counter. `trace` is a non-consuming snapshot; writing it clears captured events
and starts a new since-clear loss baseline. `dropped` is cumulative.
Enabling capture does not change ordinary I/O completion behavior. This capture is a
diagnostic tracefs facility, not a new perf event source or a general ftrace
implementation; the former empty `trace_pipe` placeholder is not exposed.

The existing guest tool includes an explicit regression for this facility:

```sh
/opt/thekernel-tests/bin/thekernel-io-uring-smoke --trace
```

It checks disabled capture, request identity and ordering, cancellation,
publication, bounded loss, and clearing. Its success marker is
`THEKERNEL_IO_URING_TRACE_OK`.
