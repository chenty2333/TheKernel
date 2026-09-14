# GMBUS, DDC/EDID transport and hotplug detect

Status: implemented on `feat/intel-gmbus` (commits `3bcb93ed`, `ad28c56a`).  This
document describes what the GMBUS transport and the hotplug block do, what is
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

`kernel/src/drm/intel/` gained two modules and one register table:

| module | responsibility |
|---|---|
| `gmbus.rs` | the pin map, the transaction state machine, the timeout, the recovery, and EDID validation |
| `gmbus/tests.rs` | a controller that can be made to fail, and 27 tests that make it |
| `hpd.rs` | hotplug enable, live connect state, board polarity |
| `hpd/tests.rs` | 10 tests over bit positions and over what is *not* written |
| `regs.rs` | `BUS`: the eleven registers these two modules own, with their access classification |

The deliverable is one function:

```rust
pub(crate) fn read_edid(regs: &RegisterWindow, pin: Pin) -> Result<EdidBytes, GmbusError>
```

It reads 128 bytes from slave `0x50`, register `0x00`, validates them — header
`00 FF FF FF FF FF FF 00`, and a checksum of zero modulo 256 — and returns them
or a named failure.  Around it:

* `read_edid_detailed` — the same, plus the observations that are not failures
  (see [Bus notes](#bus-notes)).
* `read_edid_extension` — the CTA-861 block at EEPROM address `0x80`, when the
  base block declares one.
* `probe_sink` — the reference's port-identification procedure: ask DDI A, then
  DDI B, then DDI C, and report which pin answered.
* `hpd::enable_and_read` — the PRM's procedure verbatim: enable hotplug
  detection, then read the status register once.

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

Host tests only.  `cargo test -p thekernel-kernel --lib drm::intel`: **74
passed**, of which 37 are this workstream's (27 in `gmbus::tests`, 10 in
`hpd::tests`).  The tests drive the real state
machine — the same `transfer`, `wait_for`, `failure` and `read_block` the target
would run — through `FakeController`, which is a device model layered on a real
`RegisterWindow` over an ordinary buffer.  That layering is deliberate: the
register addresses, the access rules and the bounds checks under test are the
real ones, so a write to a register the table declares read-only is refused in a
test for the same reason it would be refused on hardware.

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
  rather than a zero.

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
* **No call site.**  Within this workstream's file scope the only boot-time hook
  into `kernel/src/drm/intel/` is `mod.rs::probe_at_boot`, which the brief
  reserves to the coordinator; so `read_edid`, `probe_sink` and
  `hpd::enable_and_read` are reachable from the tests and from a later caller,
  but nothing calls them during a boot as landed.  Wiring them is one line each
  (see the "Where the display driver plugs in" note in
  [`intel-gpu-probe.md`](intel-gpu-probe.md)).

## Gaps and inferences this workstream bridges

| Item | Source | What was done |
|---|---|---|
| `GMBUS1`-`GMBUS5` are in no public Gen12 register volume (reference §9.2, §13.1 item 2) | `[I915]` only | every field mask cites the i915 symbol; nothing is trusted that a read-back cannot confirm; the one ambiguous field (a byte count above 255) is avoided rather than used |
| the index cycle's shape (reference §9.3 vs `[I915]`) | `[I915]` v6.12 and v5.15 agree with each other and disagree with the reference | the single-write form, with the command word pinned by a test |
| "program `HPD_LONG_DETECT`" (§9.5) vs a field i915 only reads | `[I915]` `intel_hotplug_irq.c` | the enable bit is written, the field is reported; recorded above |
| `0xC2000` unnamed (§13.1 item 10) | `[I915]` `i915_reg.h` | it is `SOUTH_CHICKEN1`, one inversion bit per DDI; the gap is closed |
| DDI D has no DDC pin on this part (§9.2's table says otherwise) | `[I915]` `gmbus_pins_icp` | `Pin` has no DDI D; `Ddi::D.pin()` is `None` |
| which ADL-N boards invert hotplug (§9.5, §13.2) | `[I915]` applies it on DG1 only | reported, never applied unless asked |
| EDID parsing | not this workstream | bytes are returned validated; `drm::modes` parses them when that branch lands |

## Where the next step begins

1. Wire `probe_sink` and `hpd::enable_and_read` into the boot path (one line
   each, outside this workstream's file scope), so the target machine's EDID and
   its live hotplug state reach the boot log and the debug file.
2. Enable the AUX/DDC power well for the pin pair with a monitor on it — the
   power workstream's register — which `AuxWellDown` will have named.
3. Hand `EdidBytes::as_slice()` to `drm::modes`, when that branch is in `dev`,
   for the preferred timing and the constraints check.
4. Only then does a mode exist to program, which is reference §11 phase 3.
