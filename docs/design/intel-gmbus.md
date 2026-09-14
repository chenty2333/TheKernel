# GMBUS, DDC/EDID transport and hotplug detect

Status: implemented on `dev` (commits `3bcb93ed`, `155771f8`, `eb1031a6`, the
last of which added the sink step and the boot wiring).  This document describes
what the GMBUS transport, the hotplug block and the sink step do, what is
sourced and what is inferred, what has been tested and — the part that matters
most — what has not.

The deep register reference is
[`intel-display-registers.md`](intel-display-registers.md).  Section 9 is GMBUS,
EDID and hotplug, section 11.1 is the failure-mode table this workstream
implements, and section 11 phase 2 is the bring-up step it belongs to; this file
does not restate their contents, it records what the code does with them.

The target machine is an Acer 蜂鸟mini (SQM2270) with an Intel i3-N305 (Alder
Lake-N), display device `8086:46d0`.  **Nothing in this workstream has run on
that machine.**  The evidence is host tests and the sources cited below, and
that distinction is kept explicit throughout.

## What is implemented

`kernel/src/drm/intel/` gained three modules and one register table:

| module | responsibility |
|---|---|
| `gmbus.rs` | the pin map, the transaction state machine, the timeout, the recovery, and EDID validation |
| `gmbus/tests.rs` | a controller that can be made to fail, and 27 tests that make it; the double the sink and connector tests drive too |
| `hpd.rs` | hotplug enable, live connect state, board polarity |
| `hpd/tests.rs` | 16 tests over bit positions and over what is *not* written, plus the live-state polling the after-boot watch compares |
| `sink.rs` | phase 2 at boot, and after it: find the monitor, read its EDID, ask the mode layer, enable hotplug and read it once |
| `sink/tests.rs` | 5 tests that run that whole chain against the modelled controller |
| `regs/mod.rs` | `BUS`: the ten registers these modules own, with their access classification |

### One register, one declaration, one owner

The power workstream and this one both declared `ICL_PWR_WELL_CTL_AUX2`
(`0x45444`): power because it programs wells, this workstream because a NAKed
DDC transaction is diagnosed against the well's state bit.  There is one
declaration, in the power bring-up table, because the module that programs a
register owns it.  `gmbus` reads the state bit through that declaration and
writes nothing — the only registers it ever writes are `GMBUS0`, `GMBUS1`,
`GMBUS4` and `GMBUS5`, all of them its own — so the power side's read-write
classification loosens nothing here.  `regs/mod.rs` carries a test,
`no_two_tables_declare_the_same_register`, that walks every register table —
`NAMED`, the power and clock table, the combo PHY tables, `BUS` and
`table::ALL` — and refuses a duplicate by name or by offset: a register declared
twice is two decodings, two access classifications and no answer to "who owns
this address".

The `Meaning` tags were extended by both workstreams as well, into a `BringUp`
bag on one side and `BusController`/`Hotplug`/`PowerWell` on the other.  The
finer-grained set is what the tree carries, because each names a subsystem with
a module that interprets it; `BringUp` remains for the registers with no better
home (clocks, a clock's reference fuse, DBUF slices, combo PHY registers,
workarounds, error masks) and its documentation says so.  The six power-well
registers are tagged `PowerWell`.

### The register file abstraction

GMBUS and hotplug are written against `regs::Registers` — the transport, the
retry loop, the sink probe and every hotplug read are generic over it, with thin
`&RegisterWindow` entry points that fix it to the mapped aperture — the same
trait the power, clock, PHY, pipe, output and swing sequences are written
against, rather than against MMIO.  That was not true of the first version of
this workstream — it had a `BusRegisters` of its own, the same shape as
`Registers`, which the merge made visible as duplication and which is now gone
(`07a2e718`).  The consequence for testing is the point: the transport, the sink
step and the connector's tests drive one device model, `FakeController`, which
applies the same two rules the mapped window applies — a read-only register
refuses a write, and a register outside the window has no address — so a refusal
in a test means what a refusal on hardware means.  The hotplug block's own tests
drive `regs::mock::MockRegisters`, which reads an unset word as zero and is told
which registers to refuse or hide.

The deliverable is one function:

```rust
pub(crate) fn read_edid(regs: &RegisterWindow, pin: Pin) -> Result<EdidBytes, GmbusError>
```

It reads 128 bytes from slave `0x50`, register `0x00`, validates them — header
`00 FF FF FF FF FF FF 00`, and a checksum of zero modulo 256 — and returns them
or a named failure.  Around it:

* `read_edid_detailed` — the same, plus the observations that are not failures
  (see [Bus notes](#bus-notes)).
* `read_edid_extension` — the extension block at EEPROM address `0x80`, when the
  base block declares one (the CTA-861 block is the usual one).
* `probe_sink` — the reference's port-identification procedure: ask DDI A, then
  DDI B, then DDI C, and report which pin answered.
* `hpd::enable_and_read` — the PRM's procedure verbatim: enable hotplug
  detection, then read the status register once.
* `sink::probe_one` — phase 2 of the reference's bring-up order as one pass over
  one register file: the pins, the EDID and its extension, the mode layer, then
  hotplug on every DDI.  It runs at boot from `mod.rs::bring_up_at_boot`,
  through `connect::resolve_at_boot`, on every device whose register window the
  probe mapped, and the after-boot watch re-runs it for a device whose connect
  state changed.  This is what makes the EDID reachable on the target machine.

### The handoff to the mode layer

`EdidBytes` is a validated 128-byte block and nothing more; `as_slice()` is the
byte slice `drm::modes::plan_modeset` takes.  This workstream deliberately stops
at bytes.  `kernel/src/drm/modes/` was **not in this workstream's branch** — it
landed separately (`d1732266`, polished by `81ee6e81`, both in `dev`) — and
writing a second EDID parser to fill the hole would have been worse than
deferring one commit of integration.  The integration is the call this section
predicted: `sink::probe_one` runs `plan_modeset(edid.as_slice(),
&Constraints::unlimited())` over the blocks that passed their own checks,
`connect::resolve_at_boot` carries that plan out of bring-up, and `mod.rs`
consumes it when it sets the mode at boot (`c79ddc25`).

## The pin map, and the off-by-one

GMBUS selects a pin *pair* through `GMBUS0[4:0]`, and the index is **1-based**:
`GMBUS0[4:0] = 0` disconnects the controller, and DDI A is 1.  Reference §9.2
flags this in bold because an earlier draft of that document had it zero-based.
A zero-based table does not fail loudly: it selects "no pin" and every read
returns nothing, which looks exactly like a monitor that is not plugged in.  A
test pins `Pin::DdiA.index() == 1` and that no variant has index 0.

| `GMBUS0[4:0]` | `[I915]` constant | DDC for | GPIO |
|---:|---|---|---|
| 1 | `GMBUS_PIN_1_BXT` | DDI A | `GPIOB` |
| 2 | `GMBUS_PIN_2_BXT` | DDI B | `GPIOC` |
| 3 | `GMBUS_PIN_3_BXT` | DDI C | `GPIOD` |
| 9-12 | `GMBUS_PIN_9_TC1_ICP` … `_12_TC4_ICP` | Type-C 1-4 | `GPIOJ`-`GPIOM` |

**There is no DDI D pin, and reference §9.2's table says there is.**  The row
"4 | `GMBUS_PIN_4_CNP` | DDI D | `GPIOE`" comes from a different pin table.
`[I915]`'s `gmbus_pins_icp` (`display/intel_gmbus.c:113-124`) — the table the
reference itself says applies to ADL-N, recorded as `[INF]` in item 6 of §13.2 —
has entries at 1, 2, 3 and 9-14 and none at 4.  Index 4 appears in
`gmbus_pins_cnp`, `gmbus_pins_dg1`, `gmbus_pins_dg2` and `gmbus_pins_mtp`.  On
this part DDI D therefore has a hotplug index and no DDC pin, which is why
`Pin` and `hpd::Ddi` are two types with a partial mapping rather than one.

## The transaction

Reading a block is one index cycle followed by one read: `GMBUS1` carries the
index byte, the cycle type, the byte count, the slave address, the direction and
`SW_RDY`, and the controller sends the register address and then reads the block
into `GMBUS3` four bytes at a time.

```
GMBUS0 = rate | pin                      (a read-back follows; writes are posted)
GMBUS5 = 0                               (see below)
GMBUS1 = CYCLE_INDEX | CYCLE_WAIT | index << 8 | count << 16 | addr << 1 | READ | SW_RDY
loop count/4 times:
    poll GMBUS2 until HW_RDY, or SATOER, or the deadline
    read four bytes from GMBUS3, byte 0 in bits 7:0
GMBUS1 = CYCLE_STOP | SW_RDY
poll GMBUS2 until ACTIVE clears
GMBUS0 = 0
```

### Where the reference is out of date

Reference §9.3 describes the index cycle as two separate `GMBUS1` writes: an
index cycle carrying `CYCLE_INDEX | (len << 16) | (addr << 1) | SW_RDY`, and
then the read with `CYCLE_WAIT`.  That is not what any recent i915 does.  From
at least v5.15 through v6.12 — the version the reference cites in §14.2 —
`gmbus_index_xfer` puts the **index byte in `SLAVE_INDEX[15:8]`** and combines
it with the read in a single `GMBUS1` word (`display/intel_gmbus.c:596-611`),
which `gmbus_xfer_read_chunk` writes at `:451-452` with `CYCLE_INDEX` and
`CYCLE_WAIT` both set.  This driver implements the single-write form, because
that is the code path a Gen12 part actually runs, and a test decodes the word
back field by field and pins its value: for 128 bytes from `0x50` at register
`0x00`, `GMBUS1 = 0x4680_00a1`.

### Why `GMBUS5` is written

`GMBUS5[31]` enables a two-byte index, for transfers that address more than 255
bytes into a slave.  This driver never issues one, so a `GMBUS_2BYTE_INDEX_EN`
left set by firmware would silently reinterpret the index phase of the first
read — producing a plausible-looking but wrong EDID, which is the worst failure
this module could have.  Clearing it costs one write, to zero, a value i915
itself writes on this hardware (`intel_gmbus.c:615`), and the value that was
there is recorded in the notes either way.

### Timeouts and polling

Three bounds, all of them stated rather than implied:

| Bound | Value | Why |
|---|---:|---|
| one four-byte word | 50 ms | i915's `gmbus_wait` figure (`intel_gmbus.c:367-397`, reference §2.3); at 100 kHz a word takes ~430 µs |
| the bus going idle | 10 ms | i915's `gmbus_wait_idle` figure (`:414`) |
| the whole read | 250 ms | i915 leaves the total unbounded; a driver reading an EDID from the boot path cannot.  ~20x a valid 128-byte read at 100 kHz |

Completion is polled rather than taken.  `SDE_GMBUS_ICP` exists (`SDEISR` bit
23, reference §10.5) and i915 uses it, but taking it means wiring the display
interrupt path, which is a later and separate piece of work; `GMBUS4` is
therefore left cleared so no interrupt is asked for at all.  The poll is a
bounded busy wait, not a sleep: the first EDID read can happen before there is a
scheduler to sleep on.

## Failure modes, named

Reference §11.1 is a table of what goes wrong; the code gives each row an error
rather than a bare `Err`.  The two that matter are the NAK rows, because a NAK
is not a timeout and reporting it as one sends the reader to the wrong place:

* **`AuxWellDown`** — `GMBUS2.SATOER` with the AUX/DDC power well's state bit
  reading 0.  Reference §11 phase 2.1: "Enable the AUX/DDC power well for the
  port (`ICL_PWR_WELL_CTL_AUX2 0x45444`, index 0 for AUX_A) — needed for
  GMBUS/DCC on that pin pair", and its symptom line is "*GMBUS returns NAK on
  every address, always*".  The error names the well (`AUX_A` for DDI A) and
  says what it means.  Nothing enables a well: the power workstream owns that
  write, and this module only reads the state bit.
* **`NoAck`** — the same NAK with the well up, which is a statement about the
  sink or the cable instead.

The rest: `BusStuck` (`ACTIVE` never cleared, or `STALL_TIMEOUT`), `BusFloating`
(every byte `0xff` — a bus with no device and working pull-ups, which the
reference lists separately from a bad header because the causes differ),
`ReadyTimeout`, `BusInUse`, `WindowTooSmall`, `RegisterRefused`, and one error
each for a bad header and a bad checksum.

### Recovery

The recovery is the reference's, which is i915's `intel_gmbus_reset` plus its
`clear_err` path: `GMBUS0 = 0`, `GMBUS4 = 0`, wait for the bus to go idle, then
toggle `GMBUS1.SW_CLR_INT` and release the pin (`intel_gmbus.c:209-213`,
`:680-708`).  It runs before every transaction and after every failure.

At most two attempts per pin, and what changes between them is what the
reference says to change:

| First failure | Second attempt |
|---|---|
| a NAK | the same rate — i915 retries the first message once because "passive adapters sometimes NAK the first probe" (`intel_gmbus.c:714-725`) |
| a block that answered but did not validate | 50 kHz — reference §11.1: "Re-read; if persistent, lower the rate to 100 kHz or 50 kHz" |
| the controller stopped offering data, or the bus did not go idle | the same rate, after the reset — and a second failure is the answer that a reset does not fix this bus, which is what §11.1 says to establish before reaching for bit-banging |
| a floating bus, a bus already in use, a window that does not reach the registers | no second attempt: retrying cannot change any of them |

Bit-banging is **not** implemented.  §11.1 gives the GPIO procedure as the last
resort when a reset does not clear a stuck bus; that needs the GPIO pair
registers and is a separate piece of work, and the error tells a reader that
this is where they have arrived.

### Bus notes

Not every observation is a failure.  `BusNotes` carries three: whether the
firmware left `GMBUS5` in two-byte index mode, whether `GMBUS2.INUSE` was set
before the first transaction, and how many attempts the read took with the rate
of the last one.  A *successful* read with the first of those set is a finding
about the firmware, and the target machine has no other way to report it.
`read_edid` drops the notes; `read_edid_detailed` and `probe_sink` keep them.

## Hotplug

`hpd.rs` implements the reference's §9.4 procedure, quoting the PRM:

> *"To find if a receiver was connected before hotplug was enabled, enable
> hotplug in SHOTPLUG_CTL and then read the interrupt ISR to find the live
> connect state."*

`enable_and_read(regs, ddi)` reads `SHOTPLUG_CTL_DDI`, sets only that DDI's
`HPD_ENABLE` bit, reads the register back, then reads `SDEISR` once.  Waiting
for an interrupt is the wrong first move — a monitor plugged in before boot
produces no edge — and taking an interrupt is out of scope, so `SDEIER` is never
written.

| Signal | Where | DDI A value |
|---|---|---|
| `HPD_ENABLE` | `SHOTPLUG_CTL_DDI[3:0]` per DDI (`0x8 << idx*4`) | `0x8` |
| latched detect field | `SHOTPLUG_CTL_DDI[1:0]` per DDI (`0x3 << idx*4`) | `0x3` |
| live connect state | `SDEISR` bit `16 + idx` | `1 << 16` |
| board inversion | `SOUTH_CHICKEN1` bit `15 + idx` | `1 << 15` |

### Where this does less than the reference says

Reference §9.5: "Program `HPD_LONG_DETECT` (2) unless you have a reason to want
both."  **This driver does not write that field.**  i915 never writes it either:
it reads the whole register with a no-op read-modify-write and passes the value
to `icp_ddi_port_hotplug_long_detect` to classify a hotplug event as a long pulse
(a connect) or a short one (`display/intel_hotplug_irq.c:243-254`, `:564-572`).
A field that is read to find out what happened is a status latch, and writing
`2` into it would overwrite the evidence — including the evidence
`enable_and_read` reports.  So the enable bit is written, the latched field is
reported with the meaning of its value, and this difference is deliberate.

### The polarity bit, and the `[GAP]` it closes

Reference §13.1 item 10 records a gap: "I did not identify what `0xC2000` is
named in i915; it is not a register i915 programs for this purpose."  It is
`SOUTH_CHICKEN1` (`[I915]` `i915_reg.h:3358`), and the field the DG1 PRM
describes as "bits [18:15] = `1111b`" is not one four-bit value but **one bit
per DDI**: `INVERT_DDIA_HPD` at bit 15 through `INVERT_DDID_HPD` at bit 18
(`i915_reg.h:3367-3370`).  The PRM's `1111b` is therefore "invert all four"
written as a single number, and the bit-per-DDI form is what makes it possible
to invert only the port a board actually level-shifts.

i915 applies the inversion only on DG1 — `dg1_hpd_invert` sets all four bits and
is called from `dg1_hpd_enable_detection` and `dg1_hpd_irq_setup`
(`display/intel_hotplug_irq.c:883-904`) — which is direct evidence for the
reference's `[INF]` that the PRM's note is board-specific and does not
necessarily apply to an ADL-N board.  So `polarity_inverted` reports the bit,
`set_board_inversion` changes one DDI's bit when a caller decides to, and
**nothing in this workstream calls it**.  When a hotplug status bit never
changes, the polarity bit is the first thing to look at (§11 phase 2.2), and the
status line prints it every time so that nobody has to look twice.

## What is verified, and by what

Host tests only.  The kernel's host suite, filtered to `drm::intel`: **388
passed**, 0 failed, of which 48 are this workstream's (27 in `gmbus::tests`, 16
in `hpd::tests`, 5 in `sink::tests`).  The tests drive the real code the target
would run — the same `transfer`, `wait_for`, `failure`, `read_block` and
`probe_one` — through `FakeController` for the transport and phase 2, and through
a real `RegisterWindow` over an ordinary buffer, or the shared `regs::mock`
register file, for the hotplug reads.  `FakeController` is a device model
implementing `regs::Registers` and applying the same two rules the mapped window
applies.  That is deliberate: the register addresses, the access rules and the
bounds checks under test are the real ones, so a write to a register the table
declares read-only is refused in a test for the same reason it would be refused
on hardware.

What the tests establish:

* the exact `GMBUS1` command word for an EDID read, field by field and as a
  value; the pin selection for each DDI pin; that the pin is released and the
  interrupt mask cleared afterwards; and that `GMBUS3` is never read before the
  controller offered data;
* the 1-based pin indexing, the absence of a zero index, and the AUX well bits
  against the reference's own table (including the request bits this driver
  never writes);
* a valid block returned byte for byte; a bad header and a bad checksum reported
  as distinct errors; a corrupt block retried at 50 kHz and the good block
  returned when the sink answers only at that rate;
* a NAK with the well off named as `AuxWellDown` with the right well, and a NAK
  with the well on named as `NoAck`;
* a timeout that waits its budget, a partial read that never becomes a short
  block, a stuck bus, a stalled bus, a floating bus, and a bus that NAKs once
  and answers on the retry;
* the recovery: after every failure the pin is released, the interrupt mask is
  clear and `SW_CLR_INT` was toggled;
* hotplug: the bit positions for all four DDIs against the `[I915]` values, that
  enabling one DDI disturbs no other DDI's bits and does not write the latched
  detect field, that enabling detection never touches the polarity register,
  that the pulse filter is read and never written, that a read changes no
  register, and that a window which does not reach the block is a named error
  rather than a zero;
* phase 2 end to end, through the same double: a sink advertising 1920x1080@60
  produces that timing from `plan_modeset` (`148_500` kHz, strict parse, the
  sink's own detailed timing), a port with nothing on it reports the AUX well by
  name on every pin, hotplug is enabled for all four DDIs in the same pass
  without programming a detect field, a declared extension block is read and
  parsed with its base block, and a window that does not reach the registers is
  reported as that rather than as "no monitor".

One real bug came out of writing the phase-2 test, and `07a2e718` records it:
`read_block` validated every block with the *base block's* header check, so no
EDID extension block could ever be read.  An extension begins with its own tag
byte, not with `00 FF FF FF FF FF FF 00`, and its only structural check is the
checksum; a monitor that declared a CTA-861 extension read as a monitor whose
extension could not be read, with `EdidHeader` pointing the reader at a header
an extension does not have.  Validation now takes the block kind — header for
the base block, checksum for every block — and the test in `gmbus/tests.rs`
checks both directions, with the extension carried end to end in
`sink/tests.rs`.  The bug was in the version committed before the merge, and it
was invisible until something asked for an extension — which is the argument for
the integration test rather than more unit tests of the pieces.

Clippy with the repository's deny set (`-D clippy::correctness -D
clippy::suspicious`) is clean for the crate.

## What is not verified

Everything that involves silicon.  In particular:

* **No EDID has been read from a real monitor.**  No register in this module has
  been observed on an Alder Lake-N part, and the protocol has never run against
  a real GMBUS controller.  Whether the single-write index form is what this
  part wants is exactly the kind of thing the reference's §13.4 says to settle
  on the machine.
* **The AUX/DDC power well index for each pin pair is not confirmed for this
  SKU.**  Reference §4.2's `[GAP]` — which of `AUX_C` and above exist on a given
  ADL-N part — stands.  The mapping is where the state bit *would* be read, and
  a bit that reads 0 on a well the part does not have is reported as a bit that
  reads 0.
* **Whether the live connect bit is level or edge is not independently
  established.**  The procedure is the PRM's, quoted by the reference; this
  kernel cannot test it without the machine.
* **The timeouts are chosen, not measured.**  They are grounded in i915's
  figures and in the arithmetic of 100 kHz DDC, and that is all.
* **No interrupt is taken**, so nothing here exercises `SDE_GMBUS_ICP` or an
  HPD interrupt, by design.
* **Phase 2 has never run on the machine.**  The boot step is wired
  (`mod.rs::bring_up_at_boot` → `connect::resolve_at_boot` → `sink::probe_one`),
  so the first boot of this kernel on the N305 will be the first time anything
  here has touched a display controller, and no such boot is recorded yet
  ([`n305-display-acceptance.md`](n305-display-acceptance.md)).  What the step
  prints is the finding: either a monitor on a named pin with a timing, or the
  name of the power well that reads back as off.
* **The mode layer is given `Constraints::unlimited()`** at the plan site
  (`sink::probe_one`), so the plan is a statement about the sink and not about
  the engine.  The engine's ceiling is applied later, where the clock is known:
  `modeset::set_mode` reads the CDCLK, builds `EngineLimits::at_cdclk` and calls
  `choose_mode`, which accepts the plan, replaces it with the reference's
  1920x1080@60 timing, or refuses it.  The check exists; what does not is the
  agreement between the plan and the check — the mode layer can prefer a timing
  the engine then refuses, which is an honest first-bring-up shape and not a
  settled one.

## Gaps and inferences this workstream bridges

| Item | Source | What was done |
|---|---|---|
| `GMBUS1`-`GMBUS5` are in no public Gen12 register volume (reference §9.2, §13.1 item 2) | `[I915]` only | every field mask cites the i915 symbol; nothing is trusted that a read-back cannot confirm; the one ambiguous field (a byte count above 255) is avoided rather than used |
| the index cycle's shape (reference §9.3 vs `[I915]`) | `[I915]` v6.12 and v5.15 agree with each other and disagree with the reference | the single-write form, with the command word pinned by a test |
| "program `HPD_LONG_DETECT`" (§9.5) vs a field i915 only reads | `[I915]` `intel_hotplug_irq.c` | the enable bit is written, the field is reported; recorded above |
| `0xC2000` unnamed (§13.1 item 10) | `[I915]` `i915_reg.h` | it is `SOUTH_CHICKEN1`, one inversion bit per DDI; the gap is closed |
| DDI D has no DDC pin on this part (§9.2's table says otherwise) | `[I915]` `gmbus_pins_icp` | `Pin` has no DDI D; `Ddi::D.pin()` is `None` |
| which ADL-N boards invert hotplug (§9.5, §13.2) | `[I915]` applies it on DG1 only | reported, never applied unless asked |
| EDID parsing | not this workstream | bytes are returned validated and handed to `drm::modes::plan_modeset` in `sink::probe_one`; no second parser exists |

## Where the next step begins

1. Boot it.  The first line to look for is `intel-connect:`: either a monitor on
   a named pin with a timing — which identifies the physical port — or the name
   of the AUX/DDC power well that reads back as off.
2. If the well reads off, enabling it is the power workstream's register, and
   `AuxWellDown` names the well and its state bit.
3. Everything the order does after phase 2 — the mode, the pipe, the plane, the
   proof — is implemented and recorded in its own design documents
   (`intel-modeset.md`, `intel-pipe.md`, `intel-output.md` and the rest); the
   next step left to this order is the acceptance run in
   [`n305-display-acceptance.md`](n305-display-acceptance.md), where the lines
   above are the gate.
