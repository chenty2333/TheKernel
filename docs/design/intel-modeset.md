# The Intel modeset — phases 3 to 6, and what "prove it" means here

**Target hardware:** Intel Core i3-N305 (Alder Lake-N), display device `8086:46d0`, one monitor on
HDMI, no serial port — the screen is the only output device.
**Scope:** reference `docs/design/intel-display-registers.md` §11 phases 3 to 6: choose a mode,
program a pipe and a plane and a DDI, and prove that it took. Phase 6.6 (hardening) and §10
(hotplug interrupts) are out of scope for stage 2, as the reference itself says.
**Status:** the decisions and the two hardware-independent halves are implemented on
`feat/intel-verify` — `kernel/src/drm/intel/pattern.rs` (phase 6.5's test pattern) and the mode
choice plus phase 6 prove-it verdicts in `kernel/src/drm/intel/modeset.rs`. The driver that calls
the sibling modules — `modeset::set_mode` — is specified in §7.3 and **not written yet**, because it
is the layer that calls the framebuffer, pipe and output sequences and their signatures do not exist
at the time of writing. **No part of this workstream has run against a display engine.**

Provenance: register offsets, bit positions and sequences are the reference document's; where this
document goes beyond it, the claim is marked `[INF]` and the reasoning is given.

---

## 1. Where the modeset runs

### 1.1 The call site

`drm::init_virtio_gpu()` already runs the first two steps of §11 at boot, before devfs is mounted,
so that the report reaches the console on a machine that has no other way to show one:

```
drm::init_virtio_gpu()
├── intel::probe_at_boot()          §11 phase 0 — identity, PCI, the mapped register window
├── intel::bring_up_at_boot()       §11 phase 1 — power::bring_up, then §11 phase 2 — sink::probe_at_boot
│   └── modeset step (new)          §11 phases 3 to 6, per device that has a sink and a plan
└── virtio::init()
```

The modeset is the third step of that same sequence and belongs in the same function, after the
sink: a mode chosen before the EDID is read is a mode chosen from nothing. `init_virtio_gpu` itself
does not change — its two Intel calls stay as they are, and the modeset runs at the end of
`bring_up_at_boot` where the sink report and the power state are both in hand.

### 1.2 The ways it does not run, and what each says

A machine with no Intel display, a fused-off SKU and a monitor that never answered must each
produce **one honest line**, not a half-run sequence reported as a result. The existing
`bring_up_at_boot` is written that way and the modeset keeps the pattern:

| What is wrong | How it is detected | What happens |
|---|---|---|
| No Intel device, or its register window was not mapped | `mapped_windows()` is empty (already logged today) | Nothing runs. One line, unchanged from today. |
| Phase 1 failed | `POWER_FAILURE` is set | No modeset at all: the register writes of phases 3–5 are behind the power wells, and a failure there would be reported as its own fault. |
| The display is fused off | `power::FuseState::display_disabled()` — `SFUSE_STRAP` bit 7 | One line naming the strap. Nothing is programmed. |
| Pipe A is fused off | `FuseState::pipe_mask() & 1 == 0` — `SKL_DFSM` bit 30 | One line naming the pipe fuse. §11's checklist assumes pipe A, and this driver has no second pipeline to fall back to. |
| No monitor answered, or its EDID was unusable | `ModePlan::used_edid()` is false | `ModeRefusal::NoAdvertisedMode`: **the firmware framebuffer is left exactly as firmware left it.** §11 phase 2.3 forbids proceeding past an invalid EDID, and a timing the kernel guessed turns a parse bug into a display bug. |
| The power step left no usable CDCLK | `power::PowerState::cdclk` not enabled and locked, per §11 phase 1.4 | Refuse before choosing a mode, with a line that names CDCLK. Without CDCLK no pixel clock exists, and "every mode is above the ceiling" is a true but useless way to say so. |

Each skip is one line in the log and one line in the debug file. None of them is an error: they are
the answers to *why is there nothing on the screen*, which on this machine is the only question
anybody can ask.

### 1.3 The console handover is a separate step, taken only after the verdict

The target machine has no serial port, so the screen is the kernel's only output. Stage 1 left that
screen driven by the firmware's framebuffer, which works. Stage 2 reprograms the pipe that drives
it. The rule that keeps a bad bring-up from costing the machine its only console is:

> **The Intel surface becomes the console's surface only after phase 6 has proven the pipe is
> scanning out. Programming is not handover.**

`drm::screen` already has the mechanism: candidates are consulted in rank order, `rank::DRIVER` (0)
outranks `rank::FIRMWARE` (200), and a candidate that returns `Unavailable::Failed(reason)` leaves
the console where it was and puts the reason in the log. So the gate lives in the Intel candidate's
`acquire` function, and it answers from **one value**: `ProveReport::verdict()`.

The call sequence (the coordinator wires it; the parts marked WS-1 are the framebuffer workstream's):

1. `drm::init_virtio_gpu()` → `intel::probe_at_boot()` → `intel::bring_up_at_boot()` runs §11
   phase 1, then phase 2, then the modeset step (§11 phases 3–6). The modeset stores its
   `ModesetReport` — verdict included — in a `static` beside `POWER` and `SINK`, and logs it.
2. Later, when devfs is published, `/dev/fb0`'s creation calls `screen::console_scanout()`
   (`kernel/src/pseudofs/dev/mod.rs`), which consults the registered candidates in rank order.
3. The Intel candidate's `acquire()` returns:
   * `Ok(surface)` only when `report.verdict() == ScanoutVerdict::ScanningOut`. The surface is
     WS-1's framebuffer, described by the mode the modeset programmed, as a `ScanoutSurface`.
   * `Unavailable::Failed(reason)` otherwise, where `reason` is
     `ScanoutVerdict::unavailable_reason()`: every failed check with its reading, and the first
     failure's remedy in §11's own words.
   * `Unavailable::Absent(..)` when no Intel display device was identified at all.

The ordering is what makes the gate work: the verdict exists before anything asks for the surface,
because the modeset runs from `init_virtio_gpu` and the candidate is asked at devfs publication.
Registering the candidate is not handover either; registration only enters it into the rank order.

Two things the gate does **not** do, both deliberate:

* **It does not restore the picture.** Nothing is unwound (§4.2), so a failure after `TRANSCONF`
  and the DDI have been reprogrammed can leave the panel dark even though the console is correctly
  still the firmware's. The alternative — moving the console to a surface nothing is scanning —
  would leave the kernel believing it has a display it does not have, which is worse: `/dev/fb0`
  consumers, the console and every later check would be reading a surface no one can see.
* **It does not stop the panel changing before the verdict.** The plane armed in phase 4.3 points
  pipe A at the pattern, so from that moment the panel shows the *pattern*, not the log. What the
  panel carries in each case is §4.2's table; the mitigation here is that every decision line — the
  mode, the PLL dividers, the timing registers, the surface address, the pattern geometry — is
  logged **before** `PLANE_SURF` is written, so the last thing the console shows before the panel
  changes is the driver's own summary of what it is about to program.

`[INF]` The reasoning above follows from where the console writes and when the surface is chosen;
nothing here has watched a real panel. §8 records that.

The modeset is one call from one place (`bring_up_at_boot`). If the first run on hardware shows the
panel change is not worth the pattern, commenting out that one call restores stage 1 behaviour
exactly, and this document is where the trade is recorded.

### 1.4 The failure-repaint option, if the first hardware run needs it

If the verdict is not `ScanningOut` **but the plane is armed** — a failure at 6.3 or 6.4, where the
pipe is scanning and the panel is showing the bars — the driver knows the framebuffer's geometry and
could paint a short failure banner into the very surface the pipe is scanning out, so the panel
carries the reason instead of only the pattern. The mechanism already exists in the tree:
`pseudofs::dev::early_screen::paint` is a pure function of a geometry description and a byte slice,
and `ScanoutSurface::write_pixel` writes into any surface.

This is **not implemented** and is not part of `set_mode` as specified below. It is recorded because
it is the highest-value follow-up if the first hardware run fails at phase 5: it is the difference
between a panel that shows colour bars and a panel that shows which check failed. It needs WS-1's
surface type and a decision about who owns the paint, which is why it is a question for the
coordinator rather than a decision taken here.

---

## 2. Phase 3.1: which mode

### 2.1 The two policies, and where they disagree

The mode layer (`drm::modes::select`) prefers, in order: the sink's **preferred timing** (first
detailed timing descriptor), a firmware mode the sink also advertises, established timings, standard
timings, other detailed descriptors, CTA-861 VICs, and finally a built-in fallback it warns about.

Reference §11 phase 3.1 says: *"Prefer the EDID's preferred timing (first detailed descriptor), and
prefer 1920×1080@60 if it is offered — that is 148.5 MHz, comfortably inside the HBR table, and
needs no HDMI scrambling."*

The first clauses agree exactly. They disagree only when the sink's preferred timing is something
other than 1080p60 **and** the sink also offers 1080p60: the mode layer takes the preferred timing,
§11's second clause says to prefer 1080p60.

### 2.2 The reconciliation, in order

`modeset::choose_mode(plan, edid_bytes, limits) -> ModeChoice`:

1. **The mode layer did not use the EDID** → refuse (§2.4).
2. **The mode layer's choice can be programmed** → program it, unchanged, and say whose decision it
   was. This is where §11's first clause and the mode layer agree, and it is the common case.
3. **It cannot be programmed, and the sink offers a 1920×1080@60 timing that can** → program the
   1080p60 timing, and record *what was replaced and why* in `ModeChoice::ReferencePreference`. This
   is §11's second clause doing exactly the job it was written for.
4. **Neither** → refuse, naming the timing and the reason.

An override is never silent: `ModeChoice::ReferencePreference` carries the replaced mode and the
`NotProgrammable` reason, and `ModeChoice::describe()` puts both in the log line. A sink whose
preferred timing is 4K60 (594 MHz) with 1080p60 also advertised is the case this exists for.

### 2.3 The ceiling, and why a safe mode is not second-guessed

Two limits make a timing unprogrammable, and `EngineLimits::ceiling_khz()` is the lower of them:

* **CDCLK.** The stage-2 pipe is one pixel per clock — no scaling, no pixel repetition (§11's
  preamble) — so the pixel clock cannot exceed CDCLK. The caller passes what §11 phase 1.4 left:
  `power::CdclkState`, which keeps the firmware's CDCLK when it was legal.
* **No HDMI scrambling, 300 MHz.** §8.6 records `[INF]` that HDMI at or above 300 MHz TMDS needs
  scrambling and the high TMDS character rate, and says not to enable them until the simple case
  works. §11's sequence programs no scrambling bit at all, so a mode at or above 300 MHz is a mode
  this driver cannot put on the wire. At 8 bpc — `PIPE_MISC_BPC_8`, §11 phase 5.6 — the TMDS clock
  equals the pixel clock, so the comparison is direct. `[INF]` HDMI 2.0's formal threshold is
  340 MHz; the reference's more conservative 300 MHz is used because nothing here benefits from
  discovering the difference on hardware.

Within those limits the mode layer's decision stands even when it is not 1080p60. A 2560×1440@60
native mode at 241.5 MHz is a better picture than 1080p60 and is equally safe; dropping to 1080p
because the reference *mentions* 1080p would be treating a robustness argument as a rule. The
override in step 3 fires only when the mode layer's answer cannot be programmed at all.

Three further refusals come from the sequence this driver actually implements, and each is named
rather than folded into "unsupported": a **malformed** timing (edges out of order, a zero total — it
would be a register write that cannot be programmed as written); an **interlaced** one (§11's
sequence is progressive and `timing.rs` refuses to build interlaced timing registers); and a
**pixel-repeated** one (`ModeFlags::DOUBLE_CLOCK`, where the encoder is asked to emit each pixel
twice — §11 programs no pixel repetition, so the picture would be half width at the wrong rate).

The reference's other sanity check — "the mode must fit the plane" — is **not** implemented as a
dimension limit, because the reference gives no maximum plane dimension for ADL-N and this document
will not invent one. The mode layer's `Constraints` already carry `max_hdisplay`/`max_vdisplay` and
`sink.rs` passes `Constraints::unlimited()`; tightening that call is the right place for the check,
and it is a change to a step this workstream does not own. Recorded in §8.

### 2.4 Absent or unusable EDID

The reference's whole sequence assumes a monitor answered. Where nothing answered there is no timing
to program, and `choose_mode` refuses with `ModeRefusal::NoAdvertisedMode { because, fallback }` for
all three fallback reasons — no EDID, no usable mode in the EDID, and every mode excluded by the
constraints. The built-in fallback timing is **never** programmed by this driver: it exists so that
a driver is never left without a timing, not so that a mode can be invented for a monitor that never
spoke. The refusal's log line says the firmware framebuffer is left alone, in those words.

---

## 3. Phase 6.5 first: the pattern, and when it is written

### 3.1 The ordering decision

§11 lists the pattern as step 6.5, after every register write. This driver writes it **earlier**, in
phase 3.2, immediately after the framebuffer is allocated and before the plane is armed. `[INF]` The
reason is that phase 6.5's own argument applies with more force to the *ordering* than to the
timing: if the framebuffer is filled before `PLANE_SURF` is written, then there is no window in
which a correctly programmed pipe scans out an unfilled surface, and a black screen means "nothing
is scanning" and can never mean "the driver forgot to draw". The reference's §11 phase 4.3 note
matters here too — `PLANE_SURF` is the commit — so the fill has to happen before that write, not
after it.

The alternative ordering would leave exactly one frame-worth of ambiguity that §6.5 was written to
remove, and it costs nothing to avoid.

### 3.2 What the pattern is

`pattern::fill_xrgb8888(surface, stride, width, height, frame) -> Result<PatternGeometry, PatternError>`:

* **Eight vertical bars**, fixed order: white, yellow, cyan, green, magenta, red, blue, near-black.
* **No visible pixel is ever zero.** The darkest bar is `0x0010_1010`, and the marker substitutes
  mid grey over the white bar, whose complement would be black. A black region anywhere in the
  pattern would restore the very ambiguity §6.5 exists to remove, inside the region a person is most
  likely to be looking at.
* **A marker whose position is a function of the frame counter**, drawn as the complement of the bar
  beneath it, walking a grid of its own size and wrapping. A static image cannot be told apart from
  a frozen one, and "is it scanning" is what §6.1 asks. `PatternGeometry::marker` records where it
  should be for the frame that was written, so the log and a photograph can be compared. The
  generator takes the frame as a parameter and does not advance it; who does is §7.4.
* **Arbitrary stride.** §11 phase 3.2 asks for a stride that is a multiple of 64 bytes — 256 is
  safest — so on a 1920-wide surface there are 256 bytes of padding per row. Nothing outside the
  visible columns is written, by construction and by test; a generator that assumed `width * 4`
  would shear the image, and the shear would be diagnosed as a plane-position bug for a long time.

The generator writes bytes into a caller-owned slice and does nothing else: no allocation, no
mapping, no GGTT. On the target that slice is the memory the display engine reads through the GGTT,
which is the framebuffer allocation's business (WS-1), not the pattern's.

**Measured cost** (this host, the same `pattern.rs` included verbatim by a standalone harness,
writing to a `Vec<u8>` of the target's geometry — 1920×1080, stride 7936, 20 fills each):

| Build | Per fill of 1920×1080 |
|---|---|
| `-O` | **306 µs** (20 fills in 6.11 ms) |
| unoptimised | **6.02 ms** (20 fills in 120.5 ms) |

The reference's "generated in about a second" is not a constraint this design is anywhere near.
The caveat is that this measured *cached* memory: the target's framebuffer has to be mapped
write-combining for the number to transfer, and that mapping is WS-1's.

---

## 4. Phases 3 to 5: the order, and failures

### 4.1 The order, and who owns each step

Every step is a numbered step of §11 and is called in that order. Owned by this workstream unless
marked otherwise.

| Step | Action | Owner |
|---|---|---|
| 3.1 | choose the mode (`modeset::choose_mode`) | this workstream, **done** |
| 3.2 | allocate the framebuffer, write the GGTT PTE, fill the pattern | WS-1, then `pattern::fill_xrgb8888` |
| 3.3 | compute the PLL dividers and **log them before writing** | `pll::ddi_pll_dividers` |
| 3.4 | program the timing registers (every field is `value − 1`) | `timing::timing_registers` + WS-3 |
| 4.1 | `PLANE_BUF_CFG` = `0x0FFF0000` | WS-3 |
| 4.2 | watermarks: level 0 `EN \| BLOCKS(allocation) \| LINES(31)`, others disabled | WS-3 |
| 4.3 | plane noarm registers, then `PLANE_CTL`, then `PLANE_SURF` **last** | WS-3 |
| 5.1 | PLL: dividers, power, enable, poll `LOCK` | WS-2 |
| 5.2 | DDI→PLL mapping, then `DDI_CLK_OFF` cleared in a **separate** write | WS-2 |
| 5.3 | buffer translation for the port type and swing, then power the lanes | WS-2 |
| 5.4 | `TRANS_CLK_SEL(A)` | WS-2 |
| 5.5 | `TRANS_DDI_FUNC_CTL(A)` | WS-2 |
| 5.6 | `TRANSCONF(A)` | WS-2 |
| 5.7 | `DDI_BUF_CTL(A)`, then poll `IS_IDLE == 0` | WS-2 |
| 6 | prove it (`modeset::prove_it`) | this workstream, **done** |

The order is not a preference. Planes before the transcoder, transcoder before the DDI, clock before
buffer, buffer before well — §8.6's disable sequence is the exact reverse of this and its own note
says getting it backwards is how an unkillable underrun is produced. Phase 4 arms the plane into a
pipe whose output is not up yet, which is harmless (the plane reads memory into a pipe that discards
it) and is why the pattern can be in place before anything is visible.

### 4.2 Bad cases: what is left programmed, what is unwound, and what the operator sees

This is the section the console gate in §1.3 is argued from. Every row is a real state the driver
can end in; "unwound" is answered for each, and the answer is always the same one, for the reasons
that follow the table.

The **console** column is what `drm::screen` is holding, not what is on the panel: the console is
handed over only on a `ScanningOut` verdict, so until then it is the firmware's aperture, whatever
the panel happens to be showing.

| What failed | Left programmed | Unwound | Console is on | What the panel shows | What the log says |
|---|---|---|---|---|---|
| 3.1 refusal (no usable EDID, nothing programmable, display fused off) | nothing | n/a | firmware aperture | the firmware's picture, unchanged | the refusal, naming the timing and the reason, and that the firmware framebuffer is left alone |
| 3.2 framebuffer allocation | nothing | n/a | firmware aperture | the firmware's picture | WS-1's allocation failure |
| 3.3/3.4 timings | transcoder timing registers | no | firmware aperture | the firmware's picture, unless the new totals are far enough from the firmware's for the monitor to lose sync, in which case its own "no signal" or "out of range" message | every timing register value, then the failure |
| 4.1/4.2 DDB and watermarks | the plane's buffer allocation | no | firmware aperture | the firmware's picture | the DDB allocation and the watermark levels |
| **4.3 plane armed** | plane scanning **our** surface, at the firmware's timing | no | firmware aperture | **the colour bars**, from here on — the console is not on the panel any more, because pipe A no longer scans the firmware's aperture | the mode, the pattern geometry, the surface address, and the last decision lines |
| 5.1–5.6 PLL / DDI / transcoder, partway | the output stage in whatever half-state it reached | no | firmware aperture | usually dark; possibly the bars if the timing the stage was left at happens to match the monitor, which is the "rolling image = a total off by one" signature §11 describes | the PLL's `ref`, `(P,Q,K)` and symbol rate, then the failing step |
| 5.7 `IS_IDLE` never clears | everything, and the DDI is idle | no | firmware aperture | dark. This is the closest thing to a field report for this exact bring-up (`[I915]` #10932, an N200 `46d0`), and §11's advice is to suspect the port and the wiring before the PLL | `DDI_BUF_CTL`'s raw value and §11 5.2-then-5.1 |
| 6.1 no change in `PIPEDSL` | everything | no | firmware aperture | the bars, unproven: nothing says they are being refreshed rather than being one frame that arrived and stopped | each sample with its timestamp, and §11 6.1's advice (PLL, then DDI clock, then `TRANSCONF`) |
| 6.2 `PLANE_SURFLIVE` disagrees | everything; the plane did not arm | no | firmware aperture | whatever the pipe was already scanning — the firmware's picture if the plane never took the new address, garbage if it took a wrong one | the live value, the expected value, the poll count and the time waited, and §11 4.3's advice |
| 6.3 the DDI never leaves idle | everything | no | firmware aperture | dark | the raw `DDI_BUF_CTL`, and §11 5.2-then-5.1 |
| 6.4 a latched underrun | everything; the pipe **is** scanning (6.1 passed) | no | firmware aperture | **the bars, with the marker moving** — the panel itself shows the framebuffer path works and the fault is in the fetch side | the raw `PIPESTAT` and §11 6.4's advice: go back to the watermarks (4.2) before changing anything else |
| 6 all four pass | everything | n/a | **moves to our surface** at devfs publication | the bars, then the console's own repaint as fbcon draws into the new surface | the four readings and "the display engine is scanning out" |

**Nothing is unwound, in any row.** The reasons, in order of weight:

1. **The failure's on-screen signature is the primary diagnostic.** §11's phase 5 notes are written
   to be read off the *screen*: `LOCK` never setting, a doubled or halved image, "out of range", a
   rolling image (a total off by one), a black screen with correct sync. On a machine with no serial
   port that signature is the only instrument there is. Disabling the plane to "clean up" forces
   black and destroys it.
2. **There is no saved state to restore.** This kernel never read the firmware's timings, plane
   registers or DDI state before overwriting them. A partial restore would be a guess dressed as a
   rollback, and a wrong guess is harder to reason about than a documented stop.
3. **Nothing is left in an unsafe state.** `[INF]` The display engine has no state a partial modeset
   can damage; the worst case is a dark or garbage panel, and the next boot's firmware reprograms
   everything it needs. There is nothing to protect by unwinding.
4. **The sequence stops, so nothing later is attempted.** A failure at any step is reported in full
   and ends the modeset for that device. A mode that was half programmed is not a mode.
5. **The console is *not* moved as a consequence of any of it.** The gate in §1.3 means the kernel's
   model of its own display stays true through every row above, which is the one thing unwinding
   would have been protecting and the one thing it cannot actually deliver.

---

## 5. Phase 6: prove it

`modeset::prove_it(regs, timer, target) -> ProveReport` performs all four checks and returns each
one's verdict. `ProveTarget` names the four registers (`PIPEDSL`, `PLANE_SURFLIVE`, `DDI_BUF_CTL`,
`PIPESTAT`) and the two numbers they are checked against, so the pipe/DDI workstreams' types only
have to produce registers, not conform to a type this workstream invented.

### 5.1 The checks, and the policy behind each

| Step | Check | Policy |
|---|---|---|
| 6.1 | `PIPEDSL` must change | Up to 4 samples, 5 ms apart, stopping at the first change. 5 ms is a third of a frame at 60 Hz and about 337 lines at 1080p60's 67.5 kHz line rate, so a scanning pipe cannot repeat itself. The extra samples exist because phase 5 has just enabled the transcoder and a pipe that started microseconds ago may not have advanced a line yet; the healthy path costs two reads. Equal samples throughout → `NotScanning`, with every sample in the verdict. |
| 6.2 | `PLANE_SURFLIVE` must read back the address written | Polled for up to 100 ms, because the plane arms at a frame boundary after `PLANE_SURF` is written (§5.6) and an immediate read may legitimately read zero. 100 ms is at least two frames down to 20 Hz — chosen from frame arithmetic, **not measured**. Compared on the address bits `[31:12]` only: bit 2 is the decrypt flag and the low bits are not address. |
| 6.3 | `DDI_BUF_CTL.IS_IDLE` must be 0 | Read **once**. It is a live status bit; a retry that passed after a first failure would hide the fault the check exists to report. §11 calls it the single best "is my DDI alive" bit on the chip. |
| 6.4 | `PIPESTAT` bit 31 must be clear | Read **once**. It is a latched bit, and the verdict points at §11 phase 4.2 (the watermarks) before anything else, in the reference's own words. |

Every failure carries its reading: the samples, the live-versus-expected addresses and the poll
count, the raw `DDI_BUF_CTL`, the raw `PIPESTAT`. "It failed" is not a diagnosis, and on this
machine these numbers are what a person has instead of an oscilloscope.

### 5.2 Why all four checks run even after one has failed

The phases themselves stop at the first failure, because a half-programmed mode is not a mode.
Phase 6 is different by construction: it **reads** and programs nothing, so running it to the end
cannot leave hardware in a worse state. A report that stopped at 6.1 would throw away the reading
that explains it — a dead pipe *and* a latched underrun bit says the output stage never ran, while a
dead pipe alone says something else. `ProveReport::failure()` therefore reports the first failure in
§11's order for the verdict, and the whole report is logged.

### 5.3 The line rate, from §12.3

`PIPEDSL`'s low 20 bits are the scanline. The first pair of samples that differ gives a line count
over a known interval, and `ScanEvidence::observed_line_rate_hz` is that rate.
`modeset::mode_line_rate_hz(mode)` is the rate the mode implies (pixel clock over horizontal total),
and the report prints both. This is §12.3's own point: *"the only way to verify your PLL arithmetic
against reality without a scope."*

It is **evidence, not a verdict**: the sampling interval is a poll loop over the platform clock, not
a hardware timer, so the derived rate carries a few percent of slop, and a pipe scanning at the
wrong rate is still scanning, which is what 6.1 asks. `ScanEvidence::rate_agrees()` reports agreement
within 5% — loose enough for the interval's own error, tight enough that a wrong `(P,Q,K)`, which is
wrong by a factor rather than a percent, always trips it.

### 5.4 The verdict is one value, and it is the gate

`ProveReport::verdict()` returns a `ScanoutVerdict`:

```rust
pub(crate) enum ScanoutVerdict {
    /// Every check agreed.  The only value that entitles the console to move.
    ScanningOut,
    /// At least one check did not agree, with every failure in §11's order.
    NotScanningOut(Vec<(CheckId, ProveFailure)>),
}
```

`ScanningOut` is returned only when all four readings agree; anything else carries **every** failing
check, not just the first, with the reading each was decided from. The coordinator's rule is what it
is for: `drm::screen`'s Intel candidate answers `Unavailable::Failed(verdict.unavailable_reason())`
unless this is `ScanningOut` (§1.3). The reason string names each failed step, its readings, and the
first failure's remedy — so the line in the boot log and the reason the console did not move are the
same sentence, and a test asserts on that sentence rather than on a paraphrase of it.

Why one value rather than four results: four results are four chances for a caller to check three of
them, and the failure mode of that mistake is a console drawing into a surface nobody scans while the
kernel reports a working display. The type makes the mistake unrepresentable rather than unlikely.

### 5.5 A caveat that belongs in the log: `PIPESTAT` is sticky and this kernel cannot clear it

`PIPE_FIFO_UNDERRUN_STATUS` latches, and the register table declares `PIPESTAT` **read-only**, so
this driver cannot write 1 to clear it before the plane is armed. A set bit at phase 6 may therefore
predate the modeset — firmware may have left it set while driving its own pipe. The verdict reports
the raw value and points at the watermarks, which is §11's instruction, but the honest reading is
*"an underrun is latched; it may or may not be from this modeset."* Clearing it needs a `read_write`
declaration of `PIPESTAT` plus a write-1-to-clear in phase 4, which is WS-3's register; **this is a
question for the coordinator**, recorded here so it is not discovered on hardware.

---

## 6. The report

### 6.1 The log

Phase 6 logs one line per check as it is produced — `info` when a check passed, `warn` when it did
not — and one closing line, the verdict itself, which is the same string the console gate uses. The
advice strings are the reference's own remedies, not new ones: underruns point at 4.2, an idle DDI
at 5.2 then 5.1, an unarmed plane at 4.3, a dead pipe at 5.1.

The mode decision is logged before any register is written, so that the console's last words are the
driver's summary rather than the beginning of the sequence (§1.3).

### 6.2 The debug file

`probe::report_text()` feeds `/sys/kernel/debug/dri/0/intel_gpu`, and the power and sink steps
append to it. The modeset appends too, for the reason the brief gives: on the target the boot log
scrolls away and the console is the only output device, so a mode set at t=3s has to be readable at
t=300s. `ProveReport::render()` returns the phase-6 section in the same shape `SinkReport::render()`
uses: a heading, one indented line per check with its readings, the verdict as one line, and the
first failure's advice.

`set_mode` will hold a `ModesetReport` — the mode choice, the pattern geometry, each phase's
outcome, and the `ProveReport` — in a `static` beside `POWER` and `SINK`, rendered by the same
function. It is one type so that the debug file is one read of one lock, and so that a person
reading it sees the sequence in the order it ran.

---

## 7. The interface

### 7.1 What exists now, and is tested

```rust
// kernel/src/drm/intel/pattern.rs — §11 phase 6.5
pub(crate) fn fill_xrgb8888(
    surface: &mut [u8], stride: usize, width: usize, height: usize, frame: u64,
) -> Result<PatternGeometry, PatternError>;

pub(crate) const BAR_COUNT: usize;              // 8
pub(crate) const BAR_COLORS: [u32; BAR_COUNT];
pub(crate) fn bar_boundaries(width: usize) -> [usize; BAR_COUNT + 1];
pub(crate) fn bar_index(width: usize, x: usize) -> usize;
pub(crate) fn marker_rect(width: usize, height: usize, frame: u64) -> MarkerRect;
pub(crate) const fn marker_colour(under: u32) -> u32;
pub(crate) struct PatternGeometry { stride, width, height, frame, marker }  // ::describe()

// kernel/src/drm/intel/modeset.rs — §11 phase 3.1 and phase 6
pub(crate) fn choose_mode(plan: &ModePlan, edid_bytes: &[u8], limits: EngineLimits) -> ModeChoice;
pub(crate) enum ModeChoice {
    ModeLayer { mode: Mode, reason: SelectionReason },
    ReferencePreference { mode: Mode, replaced: Mode, because: NotProgrammable },
    Refused(ModeRefusal),
}
pub(crate) struct EngineLimits { pub max_clock_khz: u32 }  // ::at_cdclk(khz), ::ceiling_khz()
pub(crate) fn programmable(mode: &Mode, limits: EngineLimits) -> Result<(), NotProgrammable>;
pub(crate) fn is_reference_timing(mode: &Mode) -> bool;
pub(crate) fn mode_line_rate_hz(mode: &Mode) -> Option<u32>;

pub(crate) struct ProveTarget { surface: u32, line_rate_hz: Option<u32>, pipedsl, plane_surflive,
                                ddi_buf_ctl, pipestat: regs::Register }   // ::pipe_a(surface, rate)
pub(crate) fn prove_it<R: Registers, T: PollTimer>(
    regs: &R, timer: &T, target: &ProveTarget,
) -> ProveReport;
pub(crate) struct ProveReport { scan, surface, ddi, underrun }
// ::verdict() -> ScanoutVerdict   <- the console gate, and the only way to ask
// ::failure(), ::failure_of(check), ::check_passed(check), ::render(), ::log()

pub(crate) enum ScanoutVerdict { ScanningOut, NotScanningOut(Vec<(CheckId, ProveFailure)>) }
// ::is_scanning_out(), ::failures(), ::unavailable_reason() -> Option<String>, ::describe()
```

`choose_mode` is pure and reads no register; `prove_it` reads registers, waits on the caller's
`PollTimer` and writes nothing. Both are driven in host tests through `regs::mock::MockRegisters`
and `gmbus::tests::FakeClock`.

### 7.2 The console candidate the coordinator wires

`screen::Candidate::new(name, rank::DRIVER, reason, acquire)` takes a *function pointer*, so the
verdict has to live in a `static` — which is where `ModesetReport` goes anyway (§6.2). The acquire
function is then the whole of the gate:

```rust
/// What the console handover asks.  `None` when no modeset ran.
fn intel_console_surface() -> Result<Arc<dyn ScanoutSurface>, Unavailable> {
    let report = MODESET.lock();                     // beside POWER and SINK
    let Some(report) = report.as_ref() else {
        return Err(Unavailable::Absent("no Intel display device was identified"));
    };
    match report.verdict() {
        // Only WS-1 can build this: the framebuffer's address, geometry and layout.
        ScanoutVerdict::ScanningOut => Ok(report.console_surface()),
        verdict => Err(Unavailable::Failed(verdict.unavailable_reason().unwrap_or_default())),
    }
}
```

Two seams this needs, and both are the coordinator's to place:

| Seam | Who | What it is |
|---|---|---|
| `ModesetReport::verdict()` | this workstream | already exists as `ProveReport::verdict()`; the aggregate report will forward to it |
| `ModesetReport::console_surface() -> Arc<dyn ScanoutSurface>` | WS-1 | the allocated framebuffer as a `ScanoutSurface` at the programmed mode's geometry — width, height, pitch and XRGB8888 layout — which is also what `/dev/fb0` and fbcon will draw through |

**Is this the interface I would have designed?** Mostly, and the one thing I would change is worth
saying rather than working around:

* The function-pointer `acquire` is what forces the verdict into a global. That is fine here — the
  modeset genuinely has one outcome per boot — but it means the gate cannot be a pure function of a
  parameter, so the *honesty* of the answer depends on the static being written before the candidate
  is asked. The ordering in §1.3 (`init_virtio_gpu` before devfs publication) is therefore
  load-bearing, not incidental, and a candidate asked before the modeset ran would report
  `Absent` — which is at least the honest answer for that moment rather than a wrong one.
* The `ScanoutSurface` trait is the right shape for the handover: it carries its own pitch and pixel
  layout, so fbcon and the pattern generator cannot disagree about the stride, which is the mistake
  §3.2 was written to prevent. `pattern::fill_xrgb8888` takes a byte slice and a stride rather than
  a `ScanoutSurface` because phase 6.5 must be able to run before anything is published and because
  writing 2 million pixels through a per-pixel trait method would be needlessly slow — measured:
  306 µs for the whole 1080p frame as a slice write. The two agree because both are told the stride.

### 7.3 What `set_mode` adds, and what it needs from the siblings

The driver is one function per device, in §4.1's order:

```rust
pub(crate) fn set_mode<R: Registers, T: PollTimer>(
    regs: &R, timer: &T, device: &sink::DeviceSink, limits: EngineLimits, /* + sibling seams */
) -> Result<ModesetReport, ModesetError>;
```

It needs, from the three sibling workstreams, capabilities rather than signatures — the coordinator
supplies the exact types:

| From | Needed | Used for |
|---|---|---|
| WS-1 GGTT/framebuffer | allocate a linear, contiguous, 4 KiB-aligned surface whose stride is a multiple of 64 bytes; write the GGTT PTE; hand back the **GGTT address as a `u32`**, a `&mut [u8]` over the memory, and later an `Arc<dyn ScanoutSurface>` over the same memory | phase 3.2, then `pattern::fill_xrgb8888`; the address is what `ProveTarget::surface` compares; the surface is what the console candidate hands over |
| WS-3 pipe/plane | program the timing registers, the DDB, the watermarks, and the plane registers in §5.6's noarm/arm order, with `PLANE_SURF` last | phases 3.4, 4.1–4.3 |
| WS-2 DDI/output | program the PLL and poll `LOCK`; map DDI→PLL and clear `DDI_CLK_OFF`; buffer translation and lane power; `TRANS_CLK_SEL`, `TRANS_DDI_FUNC_CTL`, `TRANSCONF`; `DDI_BUF_CTL` and the `IS_IDLE` poll | phase 5.1–5.7 |
| the register table | a `read_write` declaration of `PIPESTAT` **and** a write-1-to-clear before the plane is armed | §5.5's caveat |

The assembly, in words, of what `set_mode` does with them:

1. `choose_mode(plan, edid, limits)`; on `Refused`, one line and `Ok(ModesetReport)` with the
   refusal recorded — a refusal is a result, not an error, and it never reaches the console gate
   with a `ScanningOut` verdict.
2. Allocate and fill the framebuffer (the pattern, at `frame = 0`).
3. `timing_registers(mode)`; log every value; hand them to WS-3.
4. WS-3's plane sequence, ending at `PLANE_SURF`.
5. WS-2's PLL/DDI/transcoder sequence, ending at the `IS_IDLE` poll.
6. `prove_it` against `ProveTarget::pipe_a(ggtt_address, mode_line_rate_hz(mode))`.
7. `ModesetReport` with every step's outcome, the verdict, and the framebuffer handle; logged and
   stored in the static the console candidate reads.

Three rules the driver must keep, all of which come from decisions above: the pattern is written
before `PLANE_SURF` (§3.1); the decision lines are logged before the plane is armed (§1.3); and the
verdict — not the last successful register write — is what the console handover reads (§5.4).

### 7.4 Who advances the pattern's frame counter

`set_mode` writes one frame, so the marker sits still until something writes the next one. A marker
that never moves cannot answer §6.1's question by eye, so part 2 should add a bounded repaint: a
deferred-work item that rewrites the surface with `frame + 1` every few hundred milliseconds, and
stops when the console takes the surface over (or after a fixed number of frames).

The measured cost says this is affordable: 306 µs per 1920×1080 fill optimised (§3.2), so a repaint
at 2 Hz is 0.06% of one core. It is not implemented here because it needs the framebuffer handle and
the point at which the console takes over, both of which are part 2's. Until then the frame number
in the report is still what makes a single frame identifiable.

---

## 8. What is not verified

Stated rather than implied, because the difference between "tested" and "reasoned" is the difference
between a result and a guess.

* **Nothing has run on real hardware.** No register in this workstream has ever been written to a
  device. Every claim about a display engine in this document is the reference document's, and every
  claim about *this code* is a host test's.
* **The tests are over a mock.** `regs::mock::MockRegisters` is a `BTreeMap` with hooks. It
  establishes the arithmetic of the mode decision and the verdicts of phase 6; it cannot establish
  that a real `PIPEDSL` advances, that a real plane arms in under 100 ms, or that a real DDI reports
  `IS_IDLE` when it should.
* **The pattern has never been seen by a display engine.** Its bar geometry, stride independence and
  frame-to-frame difference are byte-exact on the host; whether the engine's XRGB8888 interpretation
  and the plane's tiling setting agree with the byte order written here is exactly what phase 6.5
  exists to find out.
* **Every timing constant is arithmetic, not measurement.** The 5 ms sampling interval, the four
  samples, the 100 ms arming timeout and the 5% line-rate tolerance are derived from frame rates and
  from §11's "a few milliseconds"; none was measured on this board. The 300 MHz no-scrambling
  ceiling is the reference's figure for HDMI, not a measurement of this machine's link.
* **The `PIPESTAT` underrun bit cannot be cleared** by this driver (§5.5), so a set bit at phase 6
  cannot be attributed to this modeset with certainty.
* **The console gate is reasoned, not observed.** `[INF]` The claim that the verdict keeps the
  console on the firmware's aperture comes from reading `screen.rs`'s rank order and
  `pseudofs/dev/mod.rs`'s publication path; no failed modeset has been watched on a panel. What the
  panel shows in each bad case (§4.2) is likewise a prediction, and the failure-repaint option
  (§1.4) that would improve it is not implemented.
* **The frame counter does not advance yet** (§7.4), so a mode set that succeeds writes a *static*
  pattern: the bars prove the framebuffer path, but only the report's frame number and the phase 6
  readings distinguish "scanning" from "one frame that arrived and stopped". A bounded repaint is
  specified and not written.
* **The plane's maximum dimensions are not checked** (§2.3). The mode layer's `Constraints` are the
  place for that, and `sink.rs` currently passes `Constraints::unlimited()`.
* **`set_mode` does not exist yet**, so no part of phases 3.2 to 5.7 is implemented, and the
  end-to-end test the brief asks for — a whole sequence against a mock that models a working device,
  asserting on the order of writes across all phases — is not written. It cannot be written honestly
  before the sibling modules exist.
* **The register table's `PIPESTAT` access, and whether phase 4 clears the underrun bit**, is a
  question for the coordinator rather than a decision this workstream can make.
