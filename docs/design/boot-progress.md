# Verbose-boot investigation, not a speculative console fix

A2 originally stopped at `Initialize alarm...` on 2026-10-03 hardware. The user's
2026-10-04 integrated hardware run passed that point with loglevel=7; this does
not identify a root cause or establish a fix. This dev worktree still has not
reproduced the original hardware stop. No console lock/scheduler algorithm is
changed on that assumption.

## Close-condition emulated measurement

`scripts/ci/n305-verbose-qemu-smoke.py` boots N305, eight vCPUs, **8 GiB**, KVM,
firmware-fb and loglevel=7. Eight GiB is deliberately below the machine's 16 GiB:
the development host has 30 GiB and concurrent work, so matching 16 GiB would
risk host pressure. Init really obtains DHCP and starts the existing userspace
UDP log relay. A bounded foreground localhost UDP receiver verifies the actual
`Initialize alarm` kernel record, not only the relay's READY string. QMP checks
readable `A2_USERSPACE_READY` framebuffer glyphs, and QEMU must exit cleanly.

The pre-diagnostic run passed userspace, real DHCP, glyphs and UDP alarm delivery
(12 received datagrams). This is **non-reproduction**, not performance evidence
or a hardware-fix claim. An initial harness attempt tried writing /dev/kmsg,
which is not permitted by this kernel; that oracle mistake was removed instead
of weakening unrelated permissions.

## Opt-in observations

Add `boot.progress=1` to the kernel command line. Default boots keep the observer
off. Enabled boots record only atomics:

- runtime/kernel checkpoints, including PID-1 **publication**, alarm-worker start
  and return, and init join; publication does not prove PID-1 executed;
- per-CPU initialization phase, timer IRQ count and last timer timestamp;
- screen-write scope starts/completions, bytes admitted, presentation scope
  starts/completions, mirror records and last observed console point.

`/proc/boot-progress` is a read-only snapshot (`enabled=false` on default boots).
CPU phase 7 means runtime initialization/barrier completed; timer counts establish
IRQ activity, **not** scheduler/task health. Scope counters use their initiating
CPU and the last point is approximate under migration/interleaving; it is not a
lock-owner trace. Presentation counts include gate and drawing scopes, not a
claim that that many frames reached a display. Early returns finish their scope.

Last-point vocabulary:

| Point | Observation |
| --- | --- |
| 0 | last observed scope completed |
| 1 | write entry / before geometry lookup |
| 2 | before fbcon cell mutex |
| 3 | updating cells |
| 4 | queuing trailing repaint |
| 5 | before VT presentation gate |
| 6 | before display access |
| 7 | drawing callback entered |
| 8 | row cell snapshot |
| 9 | row glyph writes |
| 10 | before active-VT lookup |
| 11 | before graphics-mode lookup |

A deferred observer is admitted after PID 1 publication but before alarm workers.
It samples every five seconds for at most 60 seconds. It sends the snapshot to
the existing bounded independent diagnostic transport and priority-3 klog, for
quiet-visible mirroring when the mirror itself still works and for eventual
netconsole/dmesg. IRQ paths do not format/log, allocate or take new locks.
Snapshots take no VT/fbcon/scheduler-state lock. There is no recovery, unsafe
scanout overwrite, alternate console implementation or permanent monitor.

If stage remains 13, boot did not return from alarm admission; it does not prove
the alarm code caused the stop. Growing started-vs-completed counts and stable
points narrow the screen path; clock/IRQ progress on other CPUs narrows whether
it is a local or broader stall. If all relevant CPUs stop, the observer may not
run. If screen is stuck and neither serial/DbC nor userspace netconsole is ready,
the report is retained but may not be externally readable: this is a stated
limitation, not a reason to bypass framebuffer ownership unsafely.

The post-diagnostic smoke additionally reads this file and requires online/timer
observations for all eight CPUs. Host tests cover atomic pending/completed states,
bounds, early-return scope completion and default-disabled behavior; relevant
KVM guest and the existing fbcon glyph oracle remain required.

**New diagnostic observations have not been validated on N305 hardware; A2's
root cause remains unknown.** Next hardware session should repeat loglevel=7
2–3 times, retain quiet fallback and pair progress output with the new RTL8168
and UART evidence. Do not claim one successful boot resolved an intermittent stop.
