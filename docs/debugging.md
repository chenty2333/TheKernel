# Kernel diagnostics and request tracing

The user terminal and kernel diagnostics have separate destinations. Standard
QEMU runs capture COM1 in `console.log` and COM2 in `kernel.log`, both under the
run directory. Interactive input, program output, and test completion markers
belong to COM1. A marker in `kernel.log` cannot make a guest test pass.

CPU boot capability reports and early clock diagnostics go to the diagnostic
port. The kernel selects that port at boot by probing the UARTs beyond COM1 in
turn -- COM2, COM3, COM4 -- and taking the first that answers, which is COM2
under QEMU; the CPU suite therefore reads its reports from `kernel.log`, while
its userspace KTAP results remain in `console.log`. A machine with none of
those three ports drains the log ring over COM1 instead, sharing the console's
bounded lock. Panic output uses a bounded emergency writer on whichever port was
selected, and it does not acquire the normal logger's locks. A machine with no
serial port at all still retains kernel logs in memory; emergency output is then
best effort.

## Kernel logs

Three things are decided about any message, and only two of them belong to the
code that writes it:

* **Severity.** A `log` level, or one of the eight syslog priorities the record
  carries when the author needs a name `log` does not have: a `\x01N` prefix on
  the message, or `klog::diagnostic_at` for a boot statement. `error`/`warn`/
  `info` map to 3/4/6, `debug` and `trace` share 7, and 0/1/2 (`emerg`, `alert`,
  `crit`) and 5 (`notice`) exist so that a console a human has quieted can still
  not hide the machine's own death report -- which goes through `klog::fatal`.
* **Target.** The module path, which selects the capture cap and nothing else.
* **Destination.** `console_loglevel` decides what the consoles show, when each
  of them reads the record. No call site picks a sink, and no message is emitted
  twice because an author wanted it on two sinks.

Normal `log` macros produce complete bounded records with severity, timestamp,
CPU, task, target, and module. Severity is independent of terminal colors.

A record's leader and its level word answer two different questions: `<4>` is how
the machine routes it, `DEBUG` is which band produced it, and a `\x01N` prefix on a
`debug!` makes the two disagree on purpose. Read together they say "a warning that
only appears if you asked for the debug band, and then bounded like one".
Producers format into fixed storage and then wait for the ring, in a bounded
critical section; only the panic reader refuses to wait, because a panic may
have interrupted whoever holds it. A dedicated task drains the serial console
when that console has work pending. It preserves partial UART writes and backs
off when output stalls. A failing console worker retires that console rather
than stopping the OS, and the other console is unaffected.

Retention is unconditional for `error`, `warn` and `info`: those records enter
the ring whether or not anything prints them. Only the `debug` band is capped,
per target, and it is bounded twice by rate limits -- per call site, and
globally, so a thousand chatty places cannot exceed the second budget either.
What a closed window refused is announced by the record that reopens it
(`** N kernel log messages suppressed **`) and counted in `messages_suppressed`.
The budgets speak only to the debug band: an `error`, `warn` or `info` record
never waits on one, because a record the kernel produces once per reclaimed page
is not something the log gets to slow the allocator down for.

That last sentence is the authoring rule, not a performance note. `error`, `warn`
and `info` are the band a machine shows without being asked, so a call site in
that band is a promise that the message is not reached twice: once per boot, once
per device, once per operator action. A site on a path a remote peer, an
unprivileged loop, a per-packet poll or a per-page reclaim can drive belongs in
the debug band or behind a rate limit, whatever its name says about it. The
choice is Linux's:

* A fault that Linux would report -- a block request that failed, an interrupt
  nobody claimed, a driver that could not allocate a transmit buffer -- keeps its
  real level and goes through `ratelimit::error_ratelimited!`,
  `warn_ratelimited!` or `info_ratelimited!` (`crates/ax/tk-ratelimit`). Each is
  `printk_ratelimited()`: a `static` state per call site, ten records per five
  seconds, and a `<site>: N callbacks suppressed` warning when the window
  reopens. It is retained and printed like any other record at that level.
* An event a peer or a user drives that Linux does not report at all -- a
  malformed frame, a packet with no route, a truncated datagram -- is plain
  `debug!`, Linux's `pr_debug`.
* Where the repetition is a machine property rather than an event -- a bus
  budget that is always exhausted, an IOAPIC that cannot deliver -- the record
  is `debug!`, or one `warn!` with a latch that says later refusals are not
  reported.

A `\x01N` prefix still raises a record above what its macro names (`error!` has
no `KERN_CRIT`), but it is not a way to hide a fault in the debug band: a
`debug!` record is not retained unless its target's debug band is open.

A machine that stops itself says so through `klog::fatal`, which retains the
notice at `KERN_EMERG` if the ring answers at once and writes it to the selected
diagnostic port on the spot, because a call site that powers the CPU off leaves no
worker alive to drain a queue. The notice begins `TheKernel fatal: `, which
`tools/qemu_runner/process.py` treats as a crash, so a verification lane reports
the reason instead of a case timeout with a silent tail.

A machine the *operator* stops says its last words through `klog::death_notice`:
the same `KERN_EMERG` priority and the same write on the spot, without the fatal
anchor, because nothing failed. `reboot(2)` uses it for the four Linux spells with
`pr_emerg` -- `Restarting system`, `Restarting system with command '%s'`, `System
halted`, `Power down` -- and `KERN_EMERG` is the choice for the reason above: it is
the only priority that survives a quieted console, so a `quiet` command line cannot
hide the words that explain why the box went dark.

The default capture level is `info`. To inspect retained logs in the guest:

```sh
dmesg
cat /proc/sys/kernel/log_filter
cat /proc/sys/kernel/log_stats
```

Change capture filtering as a guest task with `CAP_SYSLOG`:

```sh
echo 'info,tk_kernel::file::io_uring=debug' > /proc/sys/kernel/log_filter
# Reproduce the operation, then restore the normal filter.
echo info > /proc/sys/kernel/log_filter
```

The first item is the default level (`off`, `error`, `warn`, `info`, `debug`, or
`trace`). Later items are `target_prefix=level`; the longest matching prefix
wins. Each write replaces the whole filter. Up to 16 unique ASCII prefixes of
64 bytes are accepted. An invalid replacement leaves the previous filter intact.
Use the `target` field in a log record to choose a prefix. A prefix matches whole
path segments, so `tk_kernel::task` covers `tk_kernel::task::signal` and not
`tk_kernel::task_tools`.

`log_filter` caps the `debug` band and nothing else. Writing `off` means "keep no
debug records", and it does not silence the log: `error`, `warn` and `info` are
retained whatever the filter says, because keeping a record and printing one are
different decisions and `dmesg` is where you ask the later question.

The build environment's `AX_LOG` sets the initial filter, and a
`loglevel=<filter>` on the kernel command line overrides it for one boot. Module
overrides can enable debug records at runtime without rebuilding.

What reaches a *screen* is the other axis, and it is one number:
`console_loglevel`, which every console compares a record's priority against
when it reads the record. Four statements move it.

```sh
cat /proc/sys/kernel/printk           # console_loglevel and the three constants
echo 4 > /proc/sys/kernel/printk      # warn and below stop printing; errors do not
dmesg -n 1                            # only KERN_EMERG reaches the consoles
echo 5000 > /proc/sys/kernel/printk_ratelimit_ms
echo 10 > /proc/sys/kernel/printk_ratelimit_burst
```

`quiet` sets it to 4 and `debug` to a value above every priority;
`loglevel=<integer>` sets it directly, and all three take effect in the order
they appear on the command line, so `quiet loglevel=8` means 8. The proc file
accepts 0, which is genuinely silent, because Linux's entry is a plain
`proc_dointvec` with no clamp (`kernel/printk/sysctl.c:24`).
`SYSLOG_ACTION_CONSOLE_OFF` is not that: it installs level 1, the
`minimum_console_loglevel` floor (`kernel/printk/printk.c:1787`), so a
`KERN_EMERG` record still reaches the screen -- which is why a death notice is
published at priority 0. Only the first OFF saves the level it replaces, and
`SYSLOG_ACTION_CONSOLE_ON` restores it; setting a level by hand clears the
saved value, so an ON after an explicit level is a no-op rather than a
resurrection of the quietness just replaced. A silent console does not cost a
panic anything, because the panic screen reads the retained ring.

The rate-limit files are this kernel's spelling of Linux's `printk_ratelimit`,
which counts in jiffies; the window here is in milliseconds and the name says so.
The burst is passes per call site per window; the global storm guard over all
call sites at once is a constant of the ring's size and has no knob.

Neither axis alters the user terminal, and neither erases a retained record: a
console that is quiet, muted, or behind loses nothing unless the ring itself
overwrites what it had not secured.

Storage is deliberately bounded: one 256 KiB retained text ring, with at most
1024 bytes per formatted record, and every reader -- `syslog(2)`, the early boot
screen, and the two consoles -- carries a cursor into it rather than a copy of
it. A whole boot measures about 25 KiB, so the ring holds ten of them before its
front starts overwriting. A slow console therefore costs nothing until the ring
itself overwrites text the console had not reached. `log_stats` reports:
`records_dropped` (records refused by the recursion guard, which should be 0),
`diagnostic_records_dropped` and `screen_records_dropped` (records the ring
overwrote before each console had secured them), `records_truncated`,
`retention_bytes_overwritten`, sink availability, both consoles' levels,
`messages_suppressed` (what the rate limits refused), and the rate-limit window.
Missing output is not evidence that an event did not happen when the relevant
loss counter increased. See `docs/design/kernel-log-retention.md` for the paths
that can still lose a record and how each was measured.

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
and request rollback/discard. Fixed reads additionally record kernel-private
`executor_started` and `executor_returned` events around the full submission
wrapper, including security/fanotify checks and provider I/O. These use the same
ring/slot/generation identity and bounded capture; they do not add lower-layer
request state transitions. A completion accepted by the
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
