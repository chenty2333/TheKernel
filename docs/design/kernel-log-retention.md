# Kernel Log Retention — What Is Kept, What Can Be Lost, and Why

**Branch:** `fix/klog-loss` · **Base:** `feat/intel-stage2` (`72330088`)
**Problem:** a boot burst reached `/dev/kmsg` and the screen but only part of it
reached the serial log, so the same boot looked like two different boots
depending on where you read it.  `intel-klog-check.py` showed it directly: the
guest's own `cat /sys/kernel/debug/dri/0/intel_gpu` printed all nine report
lines, the host-side `kernel.log` was missing three or four of them.

This document states what the retained log is, which readers see it, what the
old code could lose and by which path, what the fix changed, and what can still
be lost.  Every number here was measured in this worktree by booting QEMU; none
of it ran on real hardware.

---

## 1. The mechanism

One producer path and four readers, all in `crates/ax/thekernel-axruntime/src/klog.rs`:

| Piece | What it is |
|---|---|
| `STORE` ring | `CAPACITY` = 64 KiB of record text, byte-addressed, wrapping. `oldest`/`end` are byte cursors; `retention_bytes_overwritten` in `/proc/sys/kernel/log_stats` reports `oldest`, i.e. how much text the ring has dropped off its front. |
| `Text` | One formatted record, at most `RECORD_BYTES` = 1024 bytes, always newline-terminated. A longer record is cut and marked ` [truncated]`, counted as `records_truncated`. |
| producers | `Logger::log`, `diagnostic()` and `record()` (the `ax_print` path). A per-CPU bit (`PRODUCING`) keeps a CPU from re-entering the path. |
| readers | `snapshot_into` / `available_from` for `syslog(2)`+`/dev/kmsg`, the framebuffer console mirror, the early boot screen (`try_snapshot_into`, which refuses rather than waits so a panic cannot hang), and the diagnostic console (`DiagnosticDrain`, the serial port at `0x2f8`). |

The ring is the log.  `kernel.log` on the host is the **diagnostic console**
output, not a copy of the ring: it is whatever `DiagnosticDrain` wrote to the
UART.  That distinction is what the original symptom was hiding in.

## 2. The loss, measured, one path at a time

Five boots of the base commit, each asking the guest for `dmesg` (the ring),
the debugfs report (the same text as one string), and `/proc/sys/kernel/log_stats`:

The script counts the Intel report's lines in three places: the debugfs file
(the same text as one string), the guest's own `dmesg` of the ring, and the
host-side `kernel.log`, which is the diagnostic console.

| Boot | Debugfs lines | Intel lines in `dmesg` | Intel lines in host `kernel.log` | Host lines missing | `records_dropped` | `diagnostic_records_dropped` |
|---|---|---|---|---|---|---|
| before-1 | 9 | 10 | 6 | 3 | 24 | 6 |
| before-2 | 9 | 10 | 6 | 3 | 30 | 5 |
| before-3 | 9 | 10 | 5 | 4 | 32 | 7 |
| before-4 | 9 | 10 | 6 | 3 | 32 | 6 |
| before-5 | 9 | 10 | 6 | 3 | 46 | 6 |

(The tenth `dmesg` line is the bring-up step's own one-line report, which the
debugfs file does not carry; it was missing from the host log too, in every
boot.)

The ring held everything in every boot (`ring_missing 0`, `retention_bytes_overwritten 0`);
the diagnostic console did not.  Three instrumented boots attributed every
dropped record to the line of code that dropped it:

| Path | Drops per boot | What it cost |
|---|---|---|
| `Store::append` refusing a record because the **console's 64-record queue** was full | 6, 7, 7 | the reported symptom.  The record stayed in the ring, so `/dev/kmsg` and the screen had it and the serial log did not. |
| `allowed()` — `FILTER.try_lock()` failing | 52, 81, 129 | one record above the capture level in one boot; the rest were `trace` records the filter rejects anyway (98% of all `log()` calls are below the capture level). |
| `producer_guard()` — a nested producer on a CPU already inside the path | 0–4 | in the one boot whose drop carried its real level, a `trace` record. |
| `append()` — `STORE.try_lock()` failing | 0 in every boot | never fired here: only accepted records take the ring lock.  Nothing bounds it, so it was fixed as well. |

Two of the candidates in the original brief were checked and are not the cause:
`records_truncated` was 0 in every boot (no 1024-byte truncation), and
`retention_bytes_overwritten` was 0 (the ring never wrapped; the whole log is
about 25 KiB of the 64 KiB).  `log::set_max_level(Trace)` is set in `init`, and
the debugfs file — which is built from the retained `ProbeReport`, not the log —
was complete in every boot, which is the control that proves the code ran.

The console queue was the interesting one because it was *not* a ring problem at
all.  `Store::append` wrote the text into the ring and then pushed a copy into
`queue: [Queued; QUEUE_RECORDS]`, a 66 KiB array of 64 record copies.  The
drain printed at most one record per turn and slept at least a millisecond
between turns, while a 16550 UART takes milliseconds per 240-byte line; during
the Intel report at t≈0.5 s the queue reached 64 of 64 and the next records
were refused.  "A console that cannot keep up does not cost ring bytes" was
true, and beside the point: it cost console bytes, permanently.

## 3. The fix

**The console is now a reader of the ring, not a second copy of it.**  It keeps
a byte cursor (`DiagnosticDrain::cursor`) and copies a record out only when it
is about to print it, so a console that is behind costs nothing: the text is
still in the ring.  The record's priority — needed by the console level filter,
and not present in arbitrary `diagnostic()` text — is kept in a
`marks: [u8; CAPACITY]` array written in lockstep with `bytes`, with the top bit
marking a record's first byte.  The copy queue and `LOST_DIAGNOSTICS` are gone:
64 KiB of marks replaces the 64 x 1048-byte queue of copies, and the hand-over
slots add 4 x 1048 bytes, so the log's static footprint grows by 2656 bytes.

Two consequences worth stating:

* `diagnostic_records_dropped` now counts something exact and different: the
  records the ring overwrote **before the console secured them**.  `append`
  knows when it is about to overwrite a byte that carries `RECORD_START`, and
  compares the console cursor with the position being overwritten.  A healthy
  boot reports 0, where it used to report 6; a non-zero value now means the
  console was more than a ring behind, not that a burst arrived too fast.
* A reader that has fallen behind resumes at the next record boundary
  (`Store::record_start`).  `oldest` can fall in the middle of a record, and
  printing the tail of a record whose head the ring dropped would be worse than
  printing nothing.

**Producers wait instead of refusing.**  `allowed()` reads the filter under
`FILTER.lock()` and `append()` writes under `STORE.lock()`, both bounded
critical sections (a scan of at most 16 prefixes; at most a 1 KiB copy).  A
record is no longer thrown away because another CPU was inside either lock at
that instant.  The panic path still uses `try_snapshot_into` and refuses rather
than waits: a panic may have interrupted the holder.

**The recursion guard hands over instead of dropping.**  A producer that finds
its CPU already inside the producer path — an interrupt handler interrupting a
log, or anything else the path itself reached — cannot take the ring, because
the lock is not reentrant and the owner may hold it.  It now puts its finished
record in a one-slot-per-CPU `DEFERRED` table and the owner publishes it on the
way out (`ProducerGuard::drop`), before re-enabling preemption.  Only a second
hand-over in the same window is refused, and `records_dropped` counts it.

## 4. After

Five boots of `7b20181b`, same script, same QEMU, same commands:

| Boot | Debugfs lines | Intel lines in `dmesg` | Intel lines in host `kernel.log` | Host lines missing | `records_dropped` | `diagnostic_records_dropped` |
|---|---|---|---|---|---|---|
| after-1 | 9 | 10 | 10 | 0 | 0 | 0 |
| after-2 | 9 | 10 | 10 | 0 | 0 | 0 |
| after-3 | 9 | 10 | 10 | 0 | 0 | 0 |
| after-4 | 9 | 10 | 10 | 0 | 0 | 0 |
| after-5 | 9 | 10 | 10 | 0 | 0 | 0 |

`records_dropped` is 0 in every boot: nothing the kernel logged was refused.
`diagnostic_records_dropped` is 0: the console never fell a ring behind.
`records_truncated` and `retention_bytes_overwritten` remain 0, as before — the
log still fits in the ring several times over.  The host log grew from 107-109
records to 114 in every boot, which is the console receiving what it used to
refuse.

## 5. What can still be lost

* **Ring wrap.**  More than 64 KiB of unread records and the front is
  overwritten.  `retention_bytes_overwritten` reports it, and
  `diagnostic_records_dropped` counts the records the console in particular
  never printed.
* **A record longer than `RECORD_BYTES`.**  It is cut and marked
  ` [truncated]`; `records_truncated` counts it.  The mark is in the text, so a
  reader can see which record it applies to.
* **A second nested record in one window.**  One slot per CPU; the second is
  refused and counted.  This needs two records produced on one CPU while a
  third is inside the producer path.
* **Records the capture filter rejects.**  Not a loss: the level is a decision,
  and it is recorded in `/proc/sys/kernel/log_filter`.

None of these was observed in the measured boots except by construction in the
tests.

## 6. Checking it by hand

```sh
# In the guest: what the ring retained.
dmesg | grep intel-gpu
# In the guest: the counters.
cat /proc/sys/kernel/log_stats
```

`records_dropped` should be 0.  `diagnostic_records_dropped` should be 0 unless
the console fell more than a ring behind.  `retention_bytes_overwritten` should
be 0 unless the boot logged more than 64 KiB.

The measurement used here is
`/home/ava/.cache/thekernel-targets/klog-loss/klog-measure.py <worktree> <state> <tag>`,
which boots the guest, has it print `dmesg`, the debugfs report and
`/proc/sys/kernel/log_stats`, and compares the Intel lines the guest rendered
with the ones that reached the host-side `kernel.log`.

## 7. What is not verified

* No real hardware.  The target machine has no serial port, so on that machine
  the diagnostic console is absent and `diagnostic_supported` is 0 — the ring
  and the framebuffer mirror are the whole story there.  The console path this
  document measures is the QEMU one.
* The `syslog(2)`/`/dev/kmsg` and framebuffer readers were not part of the
  measurement beyond `dmesg`; they were already cursor readers and are
  unchanged.
* Contention on the two locks is exercised by host tests with a helper thread
  holding each lock, not by a real second CPU under load.
