# Hotplug after boot: polling the live connect state

Status: implemented on `feat/intel-hpd`.  This document describes what the
after-boot hotplug watch detects, how it detects it, what it deliberately does
not do, and — the part that matters most on a machine with no serial port —
what has been measured and what has not.

**Nothing in this workstream has run on hardware.**  The register facts come
from the reference document and from Linux `drm/i915`; the behaviour comes from
host tests over a modelled controller.  An Alder Lake-N part has never answered
any of these reads, and the target machine has never been asked.

The deep register reference is
[`intel-display-registers.md`](intel-display-registers.md): §9.4 is the PRM's
"enable hotplug, then read the status once" procedure, §9.5 is the hotplug
register table and the board-inversion note, §10 is the interrupt register set,
and §11 phase 2.2 is the bring-up step this workstream continues past boot.
The bring-up half is described in [`intel-gmbus.md`](intel-gmbus.md); this file
does not restate it.

The target machine is an Acer 蜂鸟mini (SQM2270) with an Intel i3-N305 (Alder
Lake-N), display device `8086:46d0`, **no serial port**: the screen is the only
console, so an event that is not logged is an event nobody sees.

## The problem this solves

`hpd::enable_and_read` runs once, during bring-up (reference §11 phase 2.2), and
`hpd::live_state` can read the same bit again — but nothing looked again after
boot.  A monitor plugged in an hour later changed no register this kernel had
already read, so `probe_at_boot`/`bring_up_at_boot` described the machine as it
was at boot and nothing else ever updated that description: not the log, not
`/sys/kernel/debug/dri/0/intel_gpu`.

**The deliverable is: a monitor plugged in or unplugged after boot becomes
visible in the kernel log and in that file, and the sink is re-probed when it
happens.**  Nothing else.  In particular no mode is set, no pipe is programmed
and the console does not move — see [What is deliberately not
done](#what-is-deliberately-not-done).

## What is detected, and how

One task, one loop:

```text
        sleep 250 ms
          |
          v
   read SDEISR and SOUTH_CHICKEN1  (two uncached reads, no writes)   <-- hpd::poll_connect
          |
          v
   per DDI: connect = live_bit XOR invert_bit                        <-- hpd::decides_connect
          |
          v
   compare with the state the last answered poll left                <-- hpd::ConnectTracker
          |
     no change --------------------------------------------------> sleep again
          |
        change
          |
          v
   log the transition with both raw words                            <-- at N ms
          |
          v
   re-run phase 2 for this device: sink::probe_one                   <-- GMBUS, EDID, mode plan
          |
          v
   keep the event and the probe in the debug file                    <-- /sys/kernel/debug/dri/0/intel_gpu
```

The pieces, and where they live:

| what | where |
|---|---|
| the pure read-and-classify, `poll_connect` | `kernel/src/drm/intel/hpd.rs` |
| the edge detector, `ConnectTracker` | `kernel/src/drm/intel/hpd.rs` |
| one pass: poll, compare, re-probe (`reconcile_once`) | `kernel/src/drm/intel/mod.rs` |
| the baseline from the boot read (`boot_baseline`) | `kernel/src/drm/intel/mod.rs` |
| the task itself (`watch_hotplug`, `target_os = "none"`) | `kernel/src/drm/intel/mod.rs` |
| the events and the re-probe result, `HOTPLUG` | `kernel/src/drm/intel/mod.rs` |
| the report text | `kernel/src/drm/intel/mod.rs::report_text` → `debugfs::report` |

Three properties are deliberate, and each is a place a naive implementation
would have gone wrong:

* **The poll writes nothing at all.**  `SDEISR` is write-one-to-clear, so a poll
  that wrote it would discard the state it came to read (reference §10.7;
  `regs::SDEISR` is declared `read_only` for exactly this reason).  Detection is
  already enabled — phase 2 did that — so a poll has no reason to write
  anything, and a test asserts through the mock's write log that the poll path
  leaves it empty.
* **A read that failed is not a state.**  If either register does not answer,
  every DDI's state is `None`, no event is produced and no re-probe runs: "I
  could not look" is not "nothing is there".  A failed read also does not reset
  the baseline, so a poll that failed and then succeeded with the same value
  reports nothing.
* **Only a change re-probes.**  A steady state costs two register reads and
  nothing else — no GMBUS transaction, no allocation, no lock.  The tests
  assert the transaction count is unchanged across a poll that found no change.

### The baseline, and why the first poll is silent

The watch is seeded from `SINK`: the states the sink step read and logged during
bring-up become the baseline, so the first poll after boot finds the state it
already reported and has nothing to say.  A watch that reported its own first
look as a hotplug would put "a monitor arrived" in the log for a monitor that
was plugged in before the kernel started.

A DDI the boot read could not answer for has no baseline, and its first answered
poll is adopted in silence.  That is the same rule as the failed-read rule seen
from the other side, and `hpd::ConnectTracker` documents it where it is applied.

## The registers and bits

Every bit this workstream reads, with where the fact came from.  `idx` is the
DDI's number, `_HPD_PIN_DDI(hpd_pin) = hpd_pin - HPD_PORT_A` ([I915]
`i915_reg.h:2543`), which is 0 for DDI A.

| Register | Address | Bits | Meaning here | Source |
|---|---|---|---|---|
| `SDEISR` | `0xC4000` | `16 + idx` | `SDE_DDI_HOTPLUG_ICP`: the **live** connect state, DDI A = bit 16 … DDI D = bit 19 | [I915] `i915_reg.h:3001`; reference §9.4, §10.5 |
| `SOUTH_CHICKEN1` | `0xC2000` | `15 + idx` | `INVERT_DDIA_HPD`…`INVERT_DDID_HPD`: the board's hotplug polarity, one bit per DDI | [I915] `i915_reg.h:3358`, `:3367-3370`; reference §9.5 |
| `SHOTPLUG_CTL_DDI` | `0xC4030` | `0x8 << (idx*4)` | `HPD_ENABLE`, written once by phase 2 and **never by the poll** | [I915] `i915_reg.h:3079`; reference §11 phase 2.2 |
| `SHPD_FILTER_CNT` | `0xC4038` | — | the pulse filter; read once at boot for the report, never written | [I915] `i915_reg.h:3092`; reference §9.5 |
| `SDEIER` | `0xC400C` | — | the south display interrupt *enable*; **not touched** | [I915] `i915_reg.h:2457-2460`; reference §10.2 |
| `SDEIIR` | `0xC4008` | — | the south display interrupt identity, write-one-to-clear; **not touched** | [I915] `i915_reg.h:2499-2502`; reference §10.2, §10.7 |
| `DISPLAY_INT_CTL` | `0x44200` | `1 << 31` | `DISPLAY_IRQ_ENABLE`, the Gen12 display interrupt gate; **not touched** | [I915] `i915_reg.h:2612-2613`; reference §10.3 |
| `SHOTPLUG_CTL_TC` | `0xC4034` | `24 + _HPD_PIN_TC` | `SDE_TC_HOTPLUG_ICP`, Type-C hotplug; **not read** | [I915] `i915_reg.h:2999`, `:3087`; reference §9.5, §8.8 |
| PCI `Interrupt Line` / `Interrupt Pin` | config `0x3c` / `0x3d` | — | whether any legacy INTx route exists at all; read by the probe, in the report | PCI configuration header; `kernel/src/drm/intel/pci.rs` |

The decision is one expression, defined once in `hpd::decides_connect` and used
by both the boot read and the poll:

```text
connected = (SDEISR & live_bit(ddi)) != 0  XOR  (SOUTH_CHICKEN1 & invert_bit(ddi)) != 0
```

`XOR`, not "the inversion wins": a board whose level shifter inverts hotplug
reports a connect as a *clear* bit, so the two bits together mean what one of
them means alone everywhere else.  The kernel has no way to tell which board it
is on — the reference records the PRM's note and could not confirm which boards
it applies to (§9.5, and item 4 of §13.2) — so the boot report still states
*both* readings in words and leaves the choice to the reader.  The watch cannot
leave it to the reader: it has to compare one poll against the next, so it uses
the one answer, and says the polarity bit in every line it writes.

## The mechanism, and why this one

Two mechanisms were considered, and the simpler one was built.

| | (i) 100 Hz timer callback + worker | (ii) sleeping task, 250 ms poll — **built** |
|---|---|---|
| register reads per second | 100 | 4 |
| detection latency | ≤ 10 ms | ≤ 250 ms |
| contexts | interrupt **and** task, sharing a published flag | one task |
| what runs in interrupt context | one `SDEISR` read and an edge compare | nothing |
| what the flag has to get right | publication, consumption, and a lost-wakeup argument | — |

The reasons for (ii):

* **The work after a change is task-context work either way.**  The re-probe is
  GMBUS transactions with documented timeouts, an EDID validation, a mode plan
  and allocation.  None of that may run in an interrupt, so (i) does not remove
  any work — it adds a handoff, and with the handoff a flag whose publication
  and consumption have to be argued about.
* **The latency it buys is invisible.**  Ten milliseconds against two hundred
  and fifty is not a difference a person watching a screen can see; ten
  milliseconds is a difference between two logs.
* **The traffic it costs is real.**  A hundred uncached reads a second forever,
  against four.
* **On a machine whose only console is the screen, fewer moving parts is the
  feature.**  (ii) is one task with one loop and no state shared between
  contexts.

The interval is a constant with a name, `HOTPLUG_POLL_INTERVAL`, so the latency
is greppable rather than implied.

## Latency

Stated exactly, because on a machine with no serial port "when did it notice" is
the first thing anyone asks:

* **Detection: 0–250 ms after the electrical event**, plus whatever scheduling
  delay the worker task sees.  The poll boundary is not synchronised with the
  event, so the expected delay is about 125 ms and the worst case is one
  interval.
* **Nothing here is a real-time guarantee.**  The watch is an ordinary
  scheduled task; a busy machine delays it by however long it is delayed, and
  `axtask::sleep`'s granularity is the scheduler's, not a hardware timer's.
* **The transition reaches the log before the re-probe starts.**  A pass
  decides first and probes afterwards, so the log line saying what changed is
  written within the detection latency; the re-probe's own result arrives when
  the bus work finishes.
* **A re-probe delays the next poll.**  The pass is sequential in one task: the
  clock read, the GMBUS transactions and the report rendering all happen before
  the loop sleeps again.  When nothing is attached, three pins are asked and
  each answer is bounded by the GMBUS timeout the transport documents (see
  `intel-gmbus.md` and reference §11.1); that bound has never been measured on
  hardware, so no millisecond figure is claimed here.
* **A plug and unplug inside one interval can be missed entirely.**  `SDEISR`'s
  hotplug bits are the *live* state, not a latched event (reference §10.2:
  "ISR = live status"), so a monitor that arrives and leaves between two polls
  looks the same at both ends and produces no transition.  This is a property of
  the mechanism, not a bug in it: what a poll can see is the state, and the
  latched detect field in `SHOTPLUG_CTL_DDI` — which might have closed that
  window — is deliberately left alone, because its meaning after boot has not
  been established and this module's rule is to report it rather than act on it
  (see `hpd.rs`, "The one place this module does less than the reference says").

## Failure modes

Every one of these is a named value with a `describe()`, or a logged line, or
both.  Nothing on this path panics: the one piece of indexing in it is guarded,
and there is no `unwrap` outside tests.

| failure | what happens | what a reader sees |
|---|---|---|
| the mapped window does not reach `SDEISR` or `SOUTH_CHICKEN1` | no state for any DDI, no event, no re-probe | `HpdError::WindowTooSmall { register }`, logged **once** per distinct register — not four times a second |
| a poll read fails once and then succeeds with the same value | nothing; the baseline survived | nothing, which is the point |
| `axtask::sleep` fails (timer admission exhausted) | the task yields and polls again immediately, staying alive | nothing; the interval is not honoured for that pass |
| the task cannot be spawned | the watch is not published, so the file says it never started | one `warn!` naming the error, and the state put back |
| nothing came up in phase 1, or no window was mapped | no watch is started at all | one line saying so, and no hotplug section in the file |
| the re-probe itself fails | `sink::probe_one` never returns an error: it reports each pin's and each DDI's failure inside the result | the same `GmbusError`/`HpdError` lines the boot probe prints, under the re-probe heading |
| a connector that makes and breaks contact | every transition is logged and re-probed, at most one re-probe per poll interval | the last 32 events and a count of the ones dropped off the front |
| more than one display device came up | one watch follows the first | a line naming the device it follows |

The event list is bounded at 32 entries with the rest counted, so a loose
connector cannot grow kernel memory without bound.  That is a bound on what is
*remembered*, not storm mitigation — see below.

## What is deliberately not done

Four things, each with the reason it is not here.

### 1. An interrupt-driven hotplug

**A display interrupt cannot be taken by this kernel today**, and this is the
finding that decided the whole design rather than an omission:

* The display function's interrupt is an **MSI**: i915 reaches it through
  `pci_enable_msi`.  This kernel has no MSI/MSI-X support of any kind.
* There is **no LAPIC vector allocator** distinct from the IOAPIC pin-derived
  vector space, so an MSI's vector has nowhere to be placed.
* There is **no configuration-space write path** in this driver, deliberately:
  `kernel/src/drm/intel/pci.rs`'s `ConfigSpace` has no write method, and its
  module documentation says why — a probe that has not identified the device has
  no business reprogramming it.
* Enabling a south-display interrupt would mean *writing* `SDEIER` and
  `DISPLAY_INT_CTL`, and clearing one means writing `SDEIIR` in the PRM's
  mask → enable → unmask order (reference §10.6, §10.7).  None of those writes
  is in scope here, and a poll needs none of them because detection is already
  enabled — so the poll path writes nothing at all.

If an interrupt path is ever wanted, these are the four things it needs, and the
first question to answer is the one the probe now answers: **is there a legacy
INTx route at all?**  The probe reads PCI `Interrupt Pin` (`0x3d`) and
`Interrupt Line` (`0x3c`) and the report says what they mean, because on this
part the answer settles whether an INTx fallback exists before anyone invests in
MSI support.  This workstream reports those two bytes and acts on neither.

### 2. Storm mitigation

There is none.  A monitor whose cable makes intermittent contact produces a
transition on every poll, and every one of them is logged, re-probed and kept
(up to the 32-event bound).  That is deliberate: the first thing a flapping
connector needs is a **log of the flap**, and a watch that rate-limited or
de-glitched its own events would hide the diagnosis it exists to produce.  What
is bounded is memory, not work.  The re-probe is naturally limited to one per
poll interval because the loop is sequential.

If a real machine turns out to flap enough for this to matter, the two knobs are
`HOTPLUG_POLL_INTERVAL` (fewer polls, so fewer re-probes) and
`HOTPLUG_EVENT_LIMIT` (less memory), and the change to make is a de-glitch
window in `hpd::ConnectTracker` — a place where the "why" is already written
down.

### 3. Type-C / DKL

The watch covers DDI A–D only.  A Type-C port's hotplug lives in a different
pair of registers — `SHOTPLUG_CTL_TC` (`0xC4034`) and `SDE_TC_HOTPLUG_ICP`
(`1 << (24 + _HPD_PIN_TC)`, [I915] `i915_reg.h:2999`, `:3087`) — and reaching a
monitor on one needs the DKL PHY, which reference §8.8 defers and §13.1 item 14
records as Bspec-only by Intel's own statement.  Reporting a Type-C port as
"connected, and no sink on it" would be a claim this workstream cannot support,
so those bits are not read at all.  `hpd::Ddi` is deliberately not shared with a
Type-C type for the same reason.

### 4. A hotplug-triggered re-modeset

**Not implemented, and it is a separate feature.**  What this workstream does on
a transition is *detect, report, and re-probe the sink*; it does not program a
pipe, a PLL, the DDB or a watermark, and it does not move the console.  The
console moves only through `scanout::register(surface, Verdict::Scanning { .. })`
on reference §11 phase 6 evidence, and nothing here calls it.

Exactly what a re-modeset would have to solve, so that the next workstream does
not have to rediscover it:

1. **Who owns the console surface while the pipe is being reprogrammed.**  The
   console is the only output device on the target machine.  Taking the pipe
   down to reprogram it means a period with no console — so a failure during the
   reprogramming is a failure nobody can read.  The alternatives are to keep the
   old surface scanning while the new pipe is prepared (which needs a second
   pipe or a hardware behaviour this workstream has no evidence for) or to
   accept a blind window and make the log reach persistent storage first.
2. **Whether the framebuffer is reused or reallocated.**  A new mode can need a
   different stride and a different total size.  Reallocating means a new
   physical allocation, a GGTT update, and freeing the old one *only after* the
   pipe has stopped scanning it.  Reusing means the mode has to fit the existing
   buffer, and the console has to be re-rendered at the new geometry — a
   different problem (`fbcon`) with a different owner.
3. **That the phase-6 gate would have to run again.**  §11 phase 6 is the
   evidence that a pipe is actually scanning, and `scanout::register` demands a
   `Verdict::Scanning { .. }`.  A hotplug-triggered modeset would have to
   produce that evidence for the *new* configuration before the console could
   be moved onto it, and it would have to have a path back to the old one when
   the new one fails.
4. **The unplug case is the harder half.**  Plugging a monitor in means there is
   a new sink to mode-set for.  Unplugging the monitor the console is on means
   the pipe is scanning into nothing, there may be no sink left to mode-set for,
   and there is no obvious fallback surface.

The input to all of this already exists: the re-probe's `ModePlan` in the
hotplug report is what the mode layer chose for the sink that just arrived.

## Reporting

Two places, deliberately the same text (`report_text` renders both):

* the **log**, as it happens: `intel-hpd: hotplug at <N> ms: DDI B: disconnected
  -> connected (SDEISR 0x00090000 (live connect bit set), SOUTH_CHICKEN1
  0x00000000 (board inversion clear))`.  The raw words are in the line because
  on this machine they are the diagnostic: they distinguish a monitor that was
  unplugged from an enable bit that never took, a polarity bit the firmware set,
  and a register that answered zero for some other reason.
* `/sys/kernel/debug/dri/0/intel_gpu`, under a
  `--- hotplug after boot (reference section 11 phase 2.2) ---` heading, whose
  first line names what is being watched and what the path does — "watching
  0000:00:02.0: one SDEISR read every 250 ms.  The poll writes no register; a
  transition re-runs the phase-2 sink probe."  Then every transition kept, with
  its millisecond, how many older ones were dropped, and the sink lines of the
  phase 2 probe re-run after the last transition.

The re-probe is rendered by the **same** `DeviceSink::render_into` the boot
report uses, on purpose: a monitor found after boot and a monitor found at boot
should be readable side by side, and one renderer is what makes that true.

## One watch, one device

`bring_up_at_boot` starts one watch, for the first device whose register window
was mapped *and* whose phase-1 power came up.  A machine with a second display
device this kernel had mapped would get a line saying which device is followed
and that the others are not.  The reason is that a report with two devices in it
needs a heading per device and a way to tell two watches' timelines apart, and
the target machine has one display function (`8086:46d0`, reference §3.1).  When
a second device exists, the change is a watch per device and a heading; it is
not a redesign.

## What is verified, and by what

Host tests only, through the exact invocation the workstream was given:

```text
cargo test --locked --manifest-path kernel/Cargo.toml --tests \
  --features 'bpf,perf-sampling,axtask/test' --target x86_64-unknown-linux-gnu \
  -- --test-threads=1 drm::intel
```

**278 passed, 0 failed** (266 before this workstream, so 12 new).  The target
build is separate and necessary: the worker is `#[cfg(target_os = "none")]`, so
the host test cannot compile that half —
`python3 tools/thekernel.py lint --platform n305` is what proves it compiles,
and it is clean.

What the new tests establish:

* `poll_connect` reads both registers once each, decides all four DDIs from
  them, and its mock write log is **empty** — the poll path writes no register
  at all, `SDEISR` included;
* a hidden register produces `None` for every DDI and names the register, never
  a guess, and still writes nothing;
* a connect and a disconnect are each one transition, and the same read twice is
  not a transition — through the real `ConnectTracker`, and again end to end
  through the real phase-2 composition;
* a board-inversion bit flips the decision and the direction of the edge;
* a failed read produces no event and no re-probe, and does not reset the
  baseline, so a later successful read of the same value is still not an event;
* a first answer with no baseline is adopted without an event;
* the baseline comes from the states the boot step reported, not from the first
  poll;
* a monitor that appears after boot is seen, the sink is probed again, the
  monitor is found on the right pin, and the report carries the transition, the
  raw words, the millisecond and the sink lines; unplugging is the other edge
  and the history is kept;
* a poll that found no change starts **no GMBUS transaction** — the transaction
  count is unchanged;
* the event list is bounded and counts what it dropped.

The one half that host tests cannot reach is `watch_hotplug` itself: the
`axtask::sleep` loop, the real `RegisterWindow` over real MMIO, and the real
clock.  Its body is `reconcile_once` plus logging, and `reconcile_once` is what
the tests drive.

## What is not verified

Everything that involves silicon, and everything that involves a real
scheduler:

* **No register in this document has been read from an Alder Lake-N part.**  The
  bit positions are `[I915]`'s and the reference's, and the tests check the
  arithmetic against those tables — not against hardware.
* **Whether the live connect bit behaves as a level on this part is not
  established.**  The PRM's procedure (quoted in reference §9.4) says to read
  the status register for the live connect state, and that is what is
  implemented; the "a plug inside one interval is missed" consequence above is a
  consequence of that reading, not a measurement.
* **The polarity bit's meaning on this board is unresolved** (reference §9.5,
  item 4 of §13.2).  The code applies it; whether it should be set for this
  board is what the machine will say.
* **The latency is arithmetic, not measurement.** It is the interval plus an
  unmeasured scheduling delay.
* **The re-probe's cost is unmeasured**, and so is whether a re-probe on real
  hardware completes in a time that a person would call "immediate".
* **The 250 ms interval is a choice.**  Nothing was measured to pick it; it was
  picked because it is four reads a second and a quarter of a second.
* **The event-list bound has never been reached on hardware**, because no
  hotplug has ever been seen by this kernel.

## One more gap this closes

`docs/design/intel-display-registers.md` §10.3 calls the display interrupt gate
"the single highest-impact trap in this section", and the interrupt path is
where any future hotplug work would start.  The probe now reads the two PCI
interrupt bytes that come before all of that, and the report says what they
mean: `Interrupt Pin` (`0x3d`) says which INTx# the function can assert at all —
zero means only MSI can reach it — and `Interrupt Line` (`0x3c`) says which
legacy IRQ firmware routed that pin to.  On the target machine those two bytes
answer "is there a fallback that does not need MSI support" before anyone
builds one, and they cost two configuration reads at boot.
