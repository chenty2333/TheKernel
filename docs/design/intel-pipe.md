# The Intel pipe: timings, DDB, watermarks and the primary plane

**Status: none of this has run on the target machine.** The target is an Acer
蜂鸟mini (SQM2270) with an i3-N305 (`8086:46d0`, ADL-N, display IP version 13).
Every number below comes from `docs/design/intel-display-registers.md`, from the
cached `drm/i915` sources that document cites, or from a host test against
`regs::mock::MockRegisters`.  **No register value in this workstream has been
written to, or read from, a real Intel display engine**, and no plane has been
observed to arm, scan, or show a picture.

This document covers one workstream: reference §11 phases 3.4, 4.1, 4.2 and 4.3,
the `PIPE_MISC` write that §11 puts in step 5.6, and the pipe-side read-backs of
phase 6.1, 6.2 and 6.4.  The rest of the modeset — the PLL, the DDI, the
transcoder enable, the framebuffer and the GGTT — is other workstreams'.

## 1. What was implemented, and where

| file | what it owns |
|---|---|
| `kernel/src/drm/intel/pipe.rs` | `compute`, `program`, `prove`: the pipe's register values, the ordered write of them, and the three phase-6 reads |
| `kernel/src/drm/intel/pipe/tests.rs` | 29 host tests driving the sequence through `MockRegisters` |
| `kernel/src/drm/intel/mod.rs` | two lines: the `mod pipe;` declaration and a row in the module table |

The interface the coordinator wires into the boot sequence:

```rust
// Compute everything, write nothing.
pub(crate) fn compute(pipe: Pipe, mode: &Mode, surface: PlaneSurface)
    -> Result<PipeProgram, PipeError>;

// Write it, in order.  PLANE_SURF is the commit and is written last.
pub(crate) fn program(regs: &impl Registers, plan: &PipeProgram)
    -> Result<PipeState, PipeError>;

// Reference section 11 phase 6.1, 6.2 and 6.4, one named verdict per check.
pub(crate) fn prove(regs: &impl Registers, plan: &PipeProgram, timer: &impl PollTimer)
    -> PipeChecks;
// The same, on the machine's own clock.  This is the one the boot path calls.
pub(crate) fn prove_at_boot(regs: &impl Registers, plan: &PipeProgram) -> PipeChecks;
```

`PlaneSurface` is the seam with the framebuffer/GGTT workstream and has exactly
two fields, neither of them derived here: `ggtt_address: u64` and
`stride_bytes: u32`.  `PipeState` carries the writes that happened,
`PipeProgram::render` renders the writes that will happen (so the whole program
can be logged before the first one), and `PipeChecks::render` renders the three
verdicts.  `PipeError::describe` and the three check enums' `describe` produce
the boot-log lines.

**Nothing calls any of this yet.**  `bring_up_at_boot` in `mod.rs` runs phases 1
and 2 and stops; wiring phase 3.4 and 4 in is the coordinator's step, and the
surface address it passes has to come from the framebuffer workstream first.

## 2. The sequence, and why the order is the deliverable

| # | step | code | registers |
|---|---|---|---|
| — | output depth | `program` | `PIPE_MISC(A)` `0x70030` (read-modify-write of `[7:5]`, `[4:2]`) |
| 3.4 | timings | `compute` → `program` | `TRANS_HTOTAL/HBLANK/HSYNC/VTOTAL/VBLANK/VSYNC(A)` `0x60000`+`0x00`..`0x14`, `PIPESRC(A)` `0x6001C` |
| 4.1 | DDB | `compute` → `program` | `PLANE_BUF_CFG(A,1)` `0x7027C` |
| 4.2 | watermarks | `compute` → `program` | `PLANE_WM(A,1,0..7)` `0x70240 + level*4` |
| 4.3 | plane, `noarm` | `compute` → `program` | `PLANE_STRIDE(A,1)` `0x70188`, `PLANE_POS` `0x7018C`, `PLANE_SIZE` `0x70190` |
| 4.3 | plane, `arm` | `compute` → `program` | `PLANE_OFFSET` `0x701A4`, `PLANE_COLOR_CTL` `0x701CC`, `PLANE_CTL` `0x70180`, `PLANE_SURF` `0x7019C` |
| 6.1 | is it scanning | `prove` | `PIPEDSL(A)` `0x70000`, four samples 1 ms apart |
| 6.2 | did the plane arm | `prove` | `PLANE_SURFLIVE(A,1)` `0x701AC` |
| 6.4 | did it underrun | `prove` | `PIPESTAT(A)` `0x70024` bit 31 |

Twenty-four writes, in exactly that order, and `PLANE_SURF` is the last of them.
The order is the deliverable because every register in phase 4 is double
buffered and **the only write that makes any of them take effect is
`PLANE_SURF`**.  §5.6 states it, the PRM states it independently ("Write the
plane surface base address register to trigger update of the watermarks and
other plane double buffered registers. This should be done only after all plane
configuration is configured to match the new watermark values."), and §7.1
supplies the symptom: the reset values of the watermark registers "will not
allow the display engine to operate", so a plane committed before its
watermarks reads nothing — a black screen with correct sync, correct timings and
a correct DDI.

`program` therefore builds the entire write list before it writes anything, and
stops at the first refused write.  Both properties are asserted on the mock's
write log rather than on the return value, because the failure they prevent is
not an error: it is a plane that is committed and blank.

Three ordering details worth stating:

* **`PIPE_MISC` is written first, not in step 5.6 where §11 puts it.**  It is
  the pipe's output bit depth, it is not double buffered and it arms nothing;
  §5.6's ordering constraint is about the plane's double-buffered state, and
  writing `PIPE_MISC` before the timings is what keeps `PLANE_SURF` the last
  write of the whole sequence.  Keeping it here also means one call programs
  everything the pipe owns, and one log line shows the whole program.
  `[I915]` orders it the same way: `bdw_set_pipe_misc` runs before
  `hsw_configure_cpu_transcoder` in `intel_crtc_enable_pipe`
  (`display/intel_display.c:1719` against `:1725`), so the output depth is
  programmed before the pipe is enabled there too.
* **§5.6's `noarm` list is a grouping, not a sequence.**  It puts `PLANE_WM` and
  `PLANE_BUF_CFG` after stride/position/size, while §11 makes the DDB and the
  watermarks phases 4.1 and 4.2 and the plane registers phase 4.3.  Nothing in
  the group takes effect before `PLANE_SURF`, so the two readings cannot
  disagree; this module follows §11's phase order and keeps §5.6's absolute
  rule, that `PLANE_CTL` is written immediately before `PLANE_SURF`.
* **Polarity, the PLL, `TRANS_CLK_SEL`, `TRANSCONF`, `TRANS_DDI_FUNC_CTL` and
  `DDI_BUF_CTL` are not here.**  They are phase 5 and the output workstream's.
  The two halves meet at `PipeProgram`, not at a register.  Until `TRANSCONF` is
  written this program has configured a pipe that is not scanning, which is why
  `prove`'s first check is `PIPEDSL`.

## 3. What the values are, and where each came from

| register | value for 1920×1080@60 (CTA VIC 16, DMT 0x52) | source |
|---|---|---|
| `TRANS_HTOTAL(A)` | `0x0897_077f` = (2200−1) << 16 \| (1920−1) | `timing.rs`, §6.1 |
| `TRANS_HBLANK(A)` | `0x0897_077f` — blanking runs to the end of the line | `timing.rs`, §6.1 |
| `TRANS_HSYNC(A)` | `0x0803_07d7` = (2052−1) << 16 \| (2008−1) | `timing.rs`, §6.1 |
| `TRANS_VTOTAL(A)` | `0x0464_0437` = (1125−1) << 16 \| (1080−1) | `timing.rs`, §6.1 |
| `TRANS_VBLANK(A)` | `0x0464_0437` | `timing.rs`, §6.1 |
| `TRANS_VSYNC(A)` | `0x0440_043b` = (1089−1) << 16 \| (1084−1) | `timing.rs`, §6.1 |
| `PIPESRC(A)` | `0x077f_0437` = (1920−1) << 16 \| (1080−1) | `timing.rs`, §6.1 |
| `PLANE_BUF_CFG(A,1)` | `0x0fff_0000` = ((4096−1) << 16) \| 0 | §11 step 4.1, §7.2 |
| `PLANE_WM(A,1,0)` | `0x8007_cfff` = `EN` \| `BLOCKS(4095)` \| `LINES(31)` | §11 step 4.2, §7.3, capped — see §4 |
| `PLANE_WM(A,1,1..7)` | `0` (disabled, written rather than left alone) | §7.3 |
| `PLANE_STRIDE(A,1)` | `120` = 7680 bytes / 64 | `[I915]`, see §4 |
| `PLANE_POS(A,1)` | `0` | §11 step 4.3; one full-screen plane |
| `PLANE_SIZE(A,1)` | `0x0437_077f` = (1080−1) << 16 \| (1920−1) | §5.4 |
| `PLANE_OFFSET(A,1)` | `0` | §11 step 4.3; no panning |
| `PLANE_COLOR_CTL(A,1)` | `0` — alpha disabled, no CSC, no gamma | §5.4, §11 step 4.3 |
| `PLANE_CTL(A,1)` | `0x8400_0000` = `ENABLE` \| `FORMAT_XRGB_8888` (4 << 24) \| `TILED_LINEAR` (0) | §5.5, §11 step 4.3 |
| `PLANE_SURF(A,1)` | the GGTT address, `[31:12]` | §5.4; the parameter of `compute` |
| `PIPE_MISC(A)` | `[7:5] = 0` (8 bpc), `[4:2] = 0` (dithering off) | §8.4, §11 step 5.6 |

Three of these need more than a citation.

### 3.1 `PLANE_SIZE` is `PIPESRC` with its halves swapped

`PLANE_SIZE` is `[31:16]` = `height − 1`, `[15:0]` = `width − 1` (§5.4), which
`[I915]` writes as `PLANE_HEIGHT(src_h - 1) | PLANE_WIDTH(src_w - 1)`
(`display/skl_universal_plane.c:1294-1295`).  `PIPESRC` is the same two counts
the other way round: `WIDTH[31:16]`, `HEIGHT[15:0]` (§5.2, §6.1).  So the value
is `pipesrc.rotate_left(16)` — the same two `count − 1` fields, swapped, with no
second subtraction anywhere in this module.

This is the one place a reader is likely to write `- 1` a second time, and it is
the reason `pipe.rs` contains no timing arithmetic at all: the `value − 1`
convention lives in exactly one function in `timing.rs`, which has a round-trip
test over every mode in the kernel's DMT and CTA-861 tables.

### 3.2 The 24 writes are all of them

`PIPE_MISC` (1) + the seven timing registers (7) + `PLANE_BUF_CFG` (1) + eight
watermark levels (8) + stride, position, size (3) + offset, colour control,
control, surface (4) = 24.

The registers §5.6 lists and this sequence does **not** write are
`PLANE_KEYVAL`, `PLANE_KEYMSK`, `PLANE_KEYMAX`, `PLANE_AUX_DIST`,
`PLANE_AUX_OFFSET` and `PLANE_WM_TRANS`.  §5.6 names all six — "key registers =
0", "0 for single-plane formats" — and **no section of the reference gives any
of them an offset**.  None is in the register table (`regs/pipe.rs`'s
transcription note records the same gap) and none was invented here.  The
consequence is stated plainly: a surface with an explicit colour key, or a
planar auxiliary plane, cannot be programmed from this module.

### 3.3 `PIPE_MISC` is a read-modify-write

`[7:5]` is the port output BPC on ADL-P and later (8 bpc = 0) and `[4]` is
dithering; every other bit of the register belongs to a colour format
(`YUV420_ENABLE[27]`, `OUTPUT_COLORSPACE_YUV[11]`), an HDR mode
(`HDR_MODE_PRECISION[23]`), PSR, or pixel rounding
(`PIXEL_ROUNDING_TRUNC[8]`).  This bring-up clears only the three fields it
owns and preserves the rest, which is a deliberate choice with a concrete
reason: `[I915]`'s `bdw_set_pipe_misc` **sets** `PIXEL_ROUNDING_TRUNC` for
display version 12 and later (`display/intel_display.c:3289-3290`) and the
reference never mentions it, so a plain write of zero would clear a bit the
vendor driver deliberately sets, on the strength of no source.

Dithering is off because there is nothing to dither: an XRGB8888 surface is 8
bits per component and the sink is driven at 8 bpc.  §8.4's `[INF]` warns
against enabling what the simple case does not need.

## 4. Two defects in the reference, and one correction

### 4.1 `PLANE_STRIDE` is not in bytes (corrected, and load-bearing)

§5.4's table describes `PLANE_STRIDE` (`0x70188`) as "`[11:0]` stride in
bytes".  That cannot be right: a 1920-wide XRGB8888 surface has a 7680-byte
stride, and 7680 does not fit in twelve bits.  The mode §11 phase 3.1 prefers
could not be programmed from that description at all.

The cached `[I915]` sources state the actual encoding:

* `display/skl_universal_plane.c:671-684`, `skl_plane_stride_mult` — "The
  stride is either expressed as a multiple of 64 bytes chunks for linear
  buffers or in number of tiles for tiled buffers", returning 64 for a linear
  surface;
* `display/skl_universal_plane.c:686-697`, `skl_plane_stride` — writes
  `scanout_stride / 64`;
* `display/skl_universal_plane.c:2782-2785` — reads it back as the field times
  that multiplier;
* `display/skl_universal_plane_regs.h:109` — `PLANE_STRIDE__MASK` is `[11:0]`.

So `PLANE_STRIDE` holds 64-byte units, the largest expressible stride is
4095 × 64 = 262080 bytes, and a stride that is not a multiple of 64 bytes
cannot be expressed at all.  **That is the real reason §11 phase 3.2 says
"make the surface stride a multiple of 64 bytes (256 is safest)"** — §5.4's
table contradicts §11 phase 3.2, and the code is right.

`pipe.rs` carries the pitch in bytes, because that is what a framebuffer and a
console speak, and converts once, at `PlaneProgram::stride_field`, with a named
error for a pitch that is not a multiple of 64 (`StrideNotAMultipleOf64`) and
one for a pitch above 4095 units (`StrideTooWide`).  Nothing is truncated: 7681
bytes is refused, not silently written as 120.  This finding is the
coordinator's as well as this workstream's — WS-1 reported it independently,
from the same symbols.

**Still unverified:** that a real Gen12 plane scans out correctly with 120 in
the field.  The evidence is a vendor-driver source reading, not a measurement.

### 4.2 The generous level-0 watermark cannot name the whole allocation

§11 step 4.2 and §7.3 both say level 0's `BLOCKS` should be the plane's whole
DDB allocation, and §7.2 makes that allocation 4096 blocks.  `PLANE_WM_BLOCKS`
is `[11:0]` (§7.3; `skl_universal_plane_regs.h:325`), whose largest value is
4095.  The instruction is not expressible in the register it names.

This module writes **4095**, and the reason is stronger than "the field is
narrow": `[I915]`'s watermark computation rejects a level whose block count
reaches the DDB allocation — "Bspec says: value >= plane ddb allocation ->
invalid, hence the +1 here" (`display/skl_watermark.c:1995-1997`) — so 4095 is
both the largest legal field value and the largest value the vendor driver
would consider valid for a 4096-block allocation.  It is one block short of the
instruction and exactly at the vendor driver's ceiling.

Recorded as an `[INF]`-grade resolution: the reference's intent ("do not
under-allocate") is met, the literal number is not.

### 4.3 `PLANE_WM_LINES`' maximum: 31 or 255

§7.3 says the maximum is 31, "a hardware limit the PRM states explicitly", and
warns that the field is 13 bits wide so the hardware will accept larger values.
`[I915]`'s `skl_wm_max_lines` returns 31 only below display version 13 and
**255 from 13 on** (`display/skl_watermark.c:1858-1864`), and ADL-N reports
display version 13 (`display/intel_display_device.c:1057`, `XE_LPD_FEATURES`'s
`ip.ver = 13`).

The generous level writes 31, which is legal under either reading, so nothing
in this workstream turns on the disagreement.  It matters for the real
watermark algorithm of §7.4, which would reject levels it need not reject if it
used the smaller bound.  Not resolved here; noted for whoever implements §7.4.

**Worth flagging separately:** the reference calls this platform "Gen12 /
Xe-LP" throughout.  In i915's terms ADL-N is `XE_LPD` with display IP version
13, not 12, and version-gated behaviour is a real trap on this platform — §15's
"meta-lesson" says exactly that.  `PIPE_MISC`'s BPC field changing meaning at
version 13 ("For Display < 13, Bits 5-7 represent DITHER BPC ... ADLP+, the
bits 5-7 represent PORT OUTPUT BPC", `i915_reg.h:1717-1722`) is one example
this module depends on, and the 12-bit DDB fields of §7.2 are another.

## 5. The output depth, and why WS-2 is not writing it

§11 step 5.6 writes the pipe enable and says, in the same step, that the output
bit depth is **not** set in `TRANSCONF` on Gen12: it and dithering go in
`PIPE_MISC`.  §8.4 carries the correction and §15 lists it as one of the
document's own fixed defects.  WS-2 is told not to write the depth, so this
module does, in the same call as the rest of the pipe's program.  The
transcoder's own `TRANS_DDI_BPC` field (`TRANS_DDI_FUNC_CTL[22:20]`) is WS-2's
and is a separate setting for a separate stage.

## 6. What is not verified

Everything, on hardware.  Specifically:

* **No register in this module has been written to real silicon.**  The host
  tests drive `regs::mock::MockRegisters`, which is a `BTreeMap` with a write
  log; it proves what was written, in what order, with what values, and what
  happens when a write is refused.  It cannot prove that any value is right.
* **The timing values are checked against `timing.rs` and against VIC 16's
  published totals** (`0x0897_077f`, `0x0803_07d7`, `0x0464_0437`,
  `0x0440_043b`, `0x077f_0437`), which is arithmetic, not a measurement.
* **No plane has been observed to arm.**  `PLANE_SURFLIVE` matching what was
  written is what §11 step 6.2 asks for, and on the mock it is a `set()`.  On
  hardware a zero there means the address was rejected — §11 step 4.3 names
  alignment and an invalid GGTT entry — and a non-zero mismatch most likely
  means the firmware's surface is still live.
* **No underrun has been observed.**  Nothing here establishes that 4095 blocks
  and 31 lines are enough for a 1080p60 plane on this machine's memory system.
  §7.3 predicts the opposite: the generous level "over-allocates and may
  under-run on a busy memory system".
* **`PIPESTAT` bit 31 has never been read from a real register.**  See §7.
* **`PLANE_STRIDE`'s 64-byte encoding is a source reading**, not a measurement
  (§4.1).
* **The 4 KiB alignment and 64-byte stride rules are enforced but not
  exercised against a real allocation.**  WS-1's framebuffer must satisfy them;
  this module only refuses them.

## 7. Values I could not source

* `PLANE_KEYVAL`, `PLANE_KEYMSK`, `PLANE_KEYMAX`, `PLANE_AUX_DIST`,
  `PLANE_AUX_OFFSET`, `PLANE_WM_TRANS`: named by §5.6, no offset anywhere.  Not
  written, not declared.
* `WM_LINETIME` (`0x45270` pipe A, `0x45274` pipe B, §7.4 step 3): a pipe-level
  watermark register, needed by the real algorithm, with no offset stated for
  pipes C and D.  Not written; it is the first register to add when §7.4 is
  implemented.
* `SAGV`'s block time (PCode command `0x23`) and the memory latency of §7.4
  step 1 (PCode command `0x06`): not read.  The generous watermark is what
  stands in for them, and §7.4's `[INF]` says so.
* The DRAM-latency level-0 adjustment for 16 Gb DIMMs, which §7.5 leaves open
  for LPDDR5.  Not addressed; it would appear as marginal underruns.
* **`PIPESTAT` bit 31's semantics are ambiguous in the reference itself.**  §5.2
  and §11 step 6.4 read bit 31 as `PIPE_FIFO_UNDERRUN_STATUS`; §10.4 says the
  status half is `[15:0]` and the enable half `[30:16]`, which leaves bit 31 in
  neither; and `regs/pipe.rs`'s transcription note flags exactly this and does
  not resolve it.  This module reads bit 31 and nothing else, as §11 directs.
  **If the bit never sets on a machine that is visibly underrunning, the next
  step is to write it** — `[I915]`'s `intel_set_cpu_fifo_underrun_reporting`
  both enables and clears the FIFO-underrun report through that one bit — but
  the register table declares `PIPESTAT` read-only, so that is an addition to
  the table for the hardening pass, not something this module does quietly.
* `PLANE_STRIDE`'s unit, `PLANE_WM_LINES`' maximum and `PLANE_WM_BLOCKS`'
  ceiling are resolved from `[I915]` rather than from the reference (§4).
* Whether `PIXEL_ROUNDING_TRUNC` should be set (§3.3).  The read-modify-write
  preserves whatever is there; nothing here decides.

## 8. A gap that belongs to the surface, not to this module

WS-1 reported, and this workstream confirms by reading the same registers, that
the GGTT page-table write cannot be followed by a TLB invalidate in this
kernel: `[I915]` writes `GEN12_GUC_TLB_INV_CR` (`0xcee8`, bit 0) after a PTE
update for graphics version 12 and later (`gt/intel_ggtt.c`,
`guc_ggtt_invalidate`), that offset lies inside `FORCEWAKE_GT` per §2.1, and
`regs::RegisterWindow` refuses forcewake-gated offsets by construction.  This
kernel has no forcewake handshake and this module does not add one.

**What the display engine's own caching requires for a PTE it has never read is
not sourced anywhere in the reference document**, and this workstream could not
source it either.  Two things are worth writing down rather than guessing:

* The display engine reads through the GGTT with its own caches, and the plane
  is being enabled against a surface mapped for the first time in this boot.
  Whether the first read after a PTE write sees the new entry without an
  invalidate is exactly the question that a hardware bring-up log would answer
  in one line.
* The practical consequence for a first light-up is bounded: a failure here
  looks like `PLANE_SURFLIVE` reading zero or a garbage picture, not like a
  kernel fault, so `prove`'s phase 6.2 check will report it.  That is the
  mitigation this workstream can offer — a named symptom rather than silence.

Recorded as a gap.  If the machine shows a plane that will not arm while
`PLANE_CTL` reads back enabled, the GGTT entry and this missing invalidate are
the first two things to check, in that order.

## 9. Testing, and what the tests are worth

`cargo test` for `drm::intel` on the host: **197 passed, 0 failed**, of which 29
are this workstream's.  The interesting ones assert properties rather than
return values:

| test | property |
|---|---|
| `the_write_order_is_phase_3_4_then_4_1_then_4_2_then_4_3` | the exact 24-write sequence, `PLANE_SURF` last |
| `computing_the_write_list_writes_nothing` | the whole list is computed before the first write, and `program` performs exactly it |
| `the_plane_is_armed_only_after_the_watermark_enable_bit` | `PLANE_WM_EN` is set in the write that precedes the commit |
| `a_refused_watermark_write_leaves_the_plane_unarmed` | a refused write stops the sequence and no `PLANE_SURF` is ever written |
| `a_refused_plane_control_write_leaves_the_plane_unarmed` | the same one register before the commit |
| `the_timing_registers_are_exactly_what_timing_produced` | the written values are `timing.rs`'s, checked against VIC 16's published totals |
| `the_plane_size_is_the_source_size_with_its_halves_swapped` | `PLANE_SIZE` decodes to (1080, 1920), `PIPESRC` to (1920, 1080) |
| `the_stride_is_written_in_sixty_four_byte_units` | 7680 bytes → 120; 7681 is refused |
| `each_pipe_has_its_own_registers` | every pipe's registers are at §5.2/§5.4/§5.3's stated stride |
| `an_underrun_names_the_watermarks_and_the_ddb` | the verdict carries the `PLANE_WM` and `PLANE_BUF_CFG` values and points at §7.1 |
| `a_pipe_that_is_not_scanning_is_a_named_failure` | `PIPEDSL` that never changes is named, and the other two checks still run |
| `a_repeated_scanline_sample_is_not_a_stopped_pipe` | four samples compared pairwise, so one frame's coincidence is not a stopped pipe |
| `a_clock_that_never_advances_does_not_hang_the_check` | the pause budget bounds a misbehaving timer |
| `pipe_misc_keeps_the_bits_this_bring_up_does_not_own` | a firmware `HDR_MODE_PRECISION`/`PIXEL_ROUNDING_TRUNC` survives the read-modify-write |

The limits of that: a mock cannot fail to arm a plane for a reason the mock was
not told about, `PIPEDSL` in a mock is a counter and not a scanline, and an
underrun is a bit this test sets by hand.  **Every "passes" above means "the
sequence does what it says", never "the hardware works".**

## 10. What a first run on the machine should do with this

1. Log `PipeProgram::render(pipe_misc_before)` before programming, so the boot
   log carries the mode, the 24 values and the DDB/watermark numbers even if
   the screen stays black.
2. `program`, then WS-2's phase 5, then `prove_at_boot`.
3. Read the three verdicts in the order they are produced, because they are
   ordered by how much they tell you: no `PIPEDSL` change means the pipe is not
   scanning and nothing later matters; a `PLANE_SURFLIVE` of zero means the
   surface address was rejected; a set bit 31 means the watermarks or the DDB
   are wrong, which is §11 step 6.4's "go back to 4.2 before changing anything
   else".
4. §11 step 6.5's advice is the other half of this workstream's usefulness and
   belongs to whoever fills the framebuffer: **fill it with vertical colour
   bars, not black.**  A solid black surface is indistinguishable from every
   failure listed above.
