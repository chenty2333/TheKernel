# The Intel pipe: timings, DDB, watermarks and the primary plane

**Status: none of this has run on the target machine.** The target is an Acer
蜂鸟mini (SQM2270) with an i3-N305 (`8086:46d0`, ADL-N, display IP version 13).
Every number below comes from `docs/design/intel-display-registers.md`, from the
cached `drm/i915` sources that document cites, or from a host test against
`regs::mock::MockRegisters`.  **No register value in this workstream has been
written to, or read from, a real Intel display engine**, and no plane has been
observed to arm, scan, or show a picture.

This document covers one workstream: reference §11 phases 3.4, 4.1, 4.2 and 4.3
(split into a shadow program and an arm step, §2 below), the `PIPE_MISC` write
that §11 puts in step 5.6, and the pipe-side read-backs of phase 6.1, 6.2 and
6.4.  The rest of the modeset — the PLL, the DDI, the transcoder enable, the
framebuffer and the GGTT — is other workstreams'.

## 1. What was implemented, and where

| file | what it owns |
|---|---|
| `kernel/src/drm/intel/pipe.rs` | `compute`, `program`, `arm`, `prove`: the pipe's register values, the ordered write of the shadow program, the arm pair, and the three phase-6 reads |
| `kernel/src/drm/intel/pipe/tests.rs` | 36 host tests driving the sequence through `MockRegisters` |
| `kernel/src/drm/intel/timing.rs` | the timing table those writes come from: six timing registers, `PIPESRC`, and the ADL+ `TRANS_SET_CONTEXT_LATENCY` substitution (§4.6) |
| `kernel/src/drm/intel/regs/pipe.rs`, `regs/ddi.rs` | the register offsets and field citations |
| `kernel/src/drm/intel/mod.rs` | two lines: the `mod pipe;` declaration and a row in the module table |

The interface the coordinator wires into the boot sequence:

```rust
// Compute everything, write nothing.
pub(crate) fn compute(pipe: Pipe, mode: &Mode, surface: PlaneSurface)
    -> Result<PipeProgram, PipeError>;

// Step 1: every shadow register, in order.  Nothing here has taken effect yet.
pub(crate) fn program(regs: &impl Registers, plan: &PipeProgram)
    -> Result<PipeState, PipeError>;

// Step 2, after output::program has written TRANSCONF: the arm pair,
// PLANE_CTL then PLANE_SURF.  This is what latches everything step 1 wrote.
pub(crate) fn arm(regs: &impl Registers, plan: &PipeProgram)
    -> Result<ArmState, PipeError>;

// Reference section 11 phase 6.1, 6.2 and 6.4, one named verdict per check.
pub(crate) fn prove(regs: &impl Registers, plan: &PipeProgram, timer: &impl PollTimer)
    -> PipeChecks;
// The same, on the machine's own clock.  This is the one the boot path calls.
pub(crate) fn prove_at_boot(regs: &impl Registers, plan: &PipeProgram) -> PipeChecks;
```

`PlaneSurface` is the seam with the framebuffer/GGTT workstream and has exactly
two fields, neither of them derived here: `ggtt_address: u64` and
`stride_bytes: u32`.  `PipeState` carries the shadow writes that happened,
`ArmState` the arm pair, `PipeProgram::render` renders both lists (so the whole
sequence can be logged before the first write), and `PipeChecks::render` renders
the three verdicts.  `PipeError::describe` and the check enums' `describe`
produce the boot-log lines.

**Nothing calls any of this yet.**  `bring_up_at_boot` in `mod.rs` runs phases 1
and 2 and stops; wiring phase 3.4 and 4 in is the coordinator's step, and the
surface address it passes has to come from the framebuffer workstream first.
The boot path's order is `pipe::program`, then `output::program` (which is what
writes `TRANSCONF`), then `pipe::arm`, then `prove_at_boot` — §2 explains why the
arm is a step of its own.

## 2. The sequence, and why the order is the deliverable

| # | step | code | registers |
|---|---|---|---|
| — | output depth and arbiter slots | `program` | `PIPE_MISC(A)` `0x70030` (read-modify-write of `[7:5]`, `[4:2]`, `[8]`), `PIPE_ARB_CTL(A)` `0x70028` (`USE_PROG_SLOTS[13]`, read-modify-write) |
| 3.4 | timings | `compute` → `program` | `TRANS_SET_CONTEXT_LATENCY(A)` `0x6007C`, `TRANS_HTOTAL/HBLANK/HSYNC/VTOTAL/VBLANK/VSYNC(A)` `0x60000`+`0x00`..`0x14`, `PIPESRC(A)` `0x6001C` |
| 4.1 | DDB | `compute` → `program` | `PLANE_BUF_CFG(A,1)` `0x7027C` |
| 4.2 | watermarks | `compute` → `program` | `PLANE_WM(A,1,0..5)` `0x70240 + level*4`, `PLANE_WM_TRANS(A,1)` `0x70268`, `PLANE_WM_SAGV` `0x70258`, `PLANE_WM_SAGV_TRANS` `0x7025C` |
| 4.3 | plane, `noarm` | `compute` → `program` | `PLANE_STRIDE(A,1)` `0x70188`, `PLANE_POS` `0x7018C`, `PLANE_SIZE` `0x70190` |
| 4.3 | plane, shadow `arm` half | `compute` → `program` | `PLANE_OFFSET` `0x701A4`, `PLANE_COLOR_CTL` `0x701CC` |
| — | **the pipe starts** | `output::program` (another workstream) | `TRANSCONF(A)` `0x70008`, `TRANS_CLK_SEL`, `TRANS_DDI_FUNC_CTL`, `DDI_BUF_CTL` |
| 4.3 | **plane arm** | `arm` | `PLANE_CTL` `0x70180`, `PLANE_SURF` `0x7019C` — adjacent, in that order, last |
| 6.1 | is it scanning | `prove` | `PIPEDSL(A)` `0x70000`, four samples 1 ms apart |
| 6.2 | did the plane arm | `prove` | `PLANE_SURFLIVE(A,1)` `0x701AC`, polled for about two frame times |
| 6.4 | did it underrun | `prove` | `PIPESTAT(A)` `0x70024` bit 31 |

Twenty-five shadow writes plus the two-write arm pair: 27 in all, and
`PLANE_SURF` is the last of them.
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

Four ordering details worth stating:

* **The arm pair is a separate step, after the pipe is enabled.**  `PLANE_SURF`
  is only the arm: the shadow registers `program` writes are latched at the
  plane's update event, which is the pipe's vblank, and a disabled transcoder
  has no vblank to latch at — `[I915]` says "Until the pipe starts PIPEDSL
  reads will return a stale value" (`display/intel_display.c:478-486`).  A
  `PLANE_SURF` written before `TRANSCONF` therefore cannot take effect until
  after the pipe starts, and depending on that is depending on undocumented
  latch behaviour, which is why `arm` is a second entry point and the boot path
  calls it after `output::program`.  `[I915]` orders it the same way and more
  strongly: it enables the crtc (`intel_enable_crtc`,
  `display/intel_display.c:7200`) and arms the plane afterwards
  (`intel_update_crtc`, `:7249`), and its arm function writes the two registers
  adjacently (`display/skl_universal_plane.c:1530-1531`) because "the control
  register self-arms if the plane was previously disabled"
  (`:1525-1532`).  **The honest limit:** coreboot's libgfxinit arms *before* the
  enable and ships that order, so the reference's order is probably not fatal —
  but whether a pending pre-enable arm survives the enable is not documented
  anywhere this workstream could find, and that is exactly why this sequence no
  longer relies on it.  It also makes phase 6.2 timing-sensitive, which is why
  that check polls (§2's last row) instead of reading once.
* **`PIPE_MISC` is written first, not in step 5.6 where §11 puts it.**  It is
  the pipe's output bit depth, it is not double buffered and it arms nothing;
  §5.6's ordering constraint is about the plane's double-buffered state, and
  writing `PIPE_MISC` before the timings is what keeps `PLANE_SURF` the last
  write of the whole sequence.  Keeping it here also means one call programs
  everything the pipe owns, and one log line shows the whole program.
  `[I915]` orders it the same way: `bdw_set_pipe_misc` runs before
  `hsw_configure_cpu_transcoder` in `intel_crtc_enable_pipe`
  (`display/intel_display.c:1719` against `:1723`), so the output depth is
  programmed before the pipe is enabled there too.
* **§5.6's `noarm` list is a grouping, not a sequence.**  It puts `PLANE_WM` and
  `PLANE_BUF_CFG` after stride/position/size, while §11 makes the DDB and the
  watermarks phases 4.1 and 4.2 and the plane registers phase 4.3.  Nothing in
  the group takes effect before `PLANE_SURF`, so the two readings cannot
  disagree; this module follows §11's phase order and keeps §5.6's absolute
  rule, that `PLANE_CTL` is written immediately before `PLANE_SURF` — which the
  arm step is what makes literal, since the pair is its own write list and
  nothing can be inserted between them.
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
| `TRANS_VBLANK(A)` | `0x0464_0000` — the blanking end, with `VBLANK_START` cleared | `timing.rs`, §6.1 and §4.6 |
| `TRANS_SET_CONTEXT_LATENCY(A)` | `0` = `vblank_start − vdisplay` | `timing.rs`, `[I915]`, §4.6 |
| `TRANS_VSYNC(A)` | `0x0440_043b` = (1089−1) << 16 \| (1084−1) | `timing.rs`, §6.1 |
| `PIPESRC(A)` | `0x077f_0437` = (1920−1) << 16 \| (1080−1) | `timing.rs`, §6.1 |
| `PLANE_BUF_CFG(A,1)` | `0x0fff_0000` = ((4096−1) << 16) \| 0 | §11 step 4.1, §7.2 |
| `PLANE_WM(A,1,0)` | `0x8007_cfff` = `EN` \| `BLOCKS(4095)` \| `LINES(31)` | §11 step 4.2, §7.3, capped — see §4 |
| `PLANE_WM(A,1,1..5)` | `0` (disabled, written rather than left alone) | §7.3 |
| `PLANE_WM_TRANS(A,1)` | `0` (disabled: no transition watermark fits this level 0) | §4.5 |
| `PLANE_WM_SAGV(A,1)` | `0x8007_cfff`, level 0's value | §4.4 |
| `PLANE_WM_SAGV_TRANS(A,1)` | `0x8007_cfff`, level 0's value | §4.4 |
| `PIPE_ARB_CTL(A)` | `1 << 13` = `USE_PROG_SLOTS` | §4.7 |
| `PLANE_STRIDE(A,1)` | `120` = 7680 bytes / 64 | `[I915]`, see §4 |
| `PLANE_POS(A,1)` | `0` | §11 step 4.3; one full-screen plane |
| `PLANE_SIZE(A,1)` | `0x0437_077f` = (1080−1) << 16 \| (1920−1) | §5.4 |
| `PLANE_OFFSET(A,1)` | `0` | §11 step 4.3; no panning |
| `PLANE_COLOR_CTL(A,1)` | `0x2000` = `PLANE_COLOR_PLANE_GAMMA_DISABLE`, alpha disabled, no CSC | §5.4, §11 step 4.3, §4.7 |
| `PLANE_CTL(A,1)` | `0x9400_0000` = `ENABLE` \| `FORMAT_XRGB_8888` (4 << 24) \| `TILED_LINEAR` (0) \| `ARB_SLOTS(1)` (1 << 28) | §5.5, §11 step 4.3, §4.7 |
| `PLANE_SURF(A,1)` | the GGTT address, `[31:12]` | §5.4; the parameter of `compute` |
| `PIPE_MISC(A)` | `[7:5] = 0` (8 bpc), `[4:2] = 0` (dithering off), `[8] = 1` (pixel rounding truncated) | §8.4, §11 step 5.6, §4.7 |

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

### 3.2 The 25 shadow writes and the arm pair are all of them

`PIPE_MISC` (1) + `PIPE_ARB_CTL` (1) + the eight timing values (8) +
`PLANE_BUF_CFG` (1) + six watermark levels, `PLANE_WM_TRANS` and the SAGV pair
(9) + stride, position, size (3) + offset and colour control (2) = 25 shadow
writes; then the arm pair, `PLANE_CTL` and `PLANE_SURF` = 27 in all.

The registers §5.6 lists and this sequence does **not** write are
`PLANE_KEYVAL`, `PLANE_KEYMSK`, `PLANE_KEYMAX`, `PLANE_AUX_DIST` and
`PLANE_AUX_OFFSET`.  §5.6 names all five — "key registers = 0", "0 for
single-plane formats" — and **no section of the reference gives any of them an
offset**.  None is in the register table (`regs/pipe.rs`'s transcription note
records the same gap) and none was invented here.  The sixth register §5.6
names without an offset, `PLANE_WM_TRANS`, is no longer in that list: `[I915]`
has its offset and this module writes it — see §4.5.

The consequence for the other five is stated plainly: a surface with an explicit
colour key, or a planar auxiliary plane, cannot be programmed from this module.

### 3.3 `PIPE_MISC` is a read-modify-write

`[7:5]` is the port output BPC on ADL-P and later (8 bpc = 0), `[4]` is
dithering and `[8]` is pixel rounding truncation; every other bit of the
register belongs to a colour format (`YUV420_ENABLE[27]`,
`OUTPUT_COLORSPACE_YUV[11]`), an HDR mode (`HDR_MODE_PRECISION[23]`) or PSR.
This bring-up owns `[7:5]`, `[4:2]` and `[8]` and preserves the rest, which is a
deliberate choice with a concrete reason: `HDR_MODE_PRECISION` is a real
firmware setting the reference lists and never says whether to set, so writing
the whole register would clear it on the strength of no source.

Bit 8 is *set* rather than preserved, because `[I915]`'s `bdw_set_pipe_misc` sets
`PIXEL_ROUNDING_TRUNC` for display version 12 and later
(`display/intel_display.c:3289-3290`, field at `i915_reg.h:1719`) and this
platform is version 13 — see §4.7.  An earlier draft of this module left the bit
alone on the grounds that the reference never mentions it; "the reference never
mentions it" is a reason not to *guess*, not a reason to leave a bit the vendor
driver sets for this exact display version at whatever firmware left.

Dithering is off because there is nothing to dither: an XRGB8888 surface is 8
bits per component and the sink is driven at 8 bpc.  §8.4's `[INF]` warns
against enabling what the simple case does not need.

`PIPE_MISC` has **no pipe-selection field** in i915 v6.12 — `PIPE_MISC(pipe)`
picks between the A and B instances and nothing more (`i915_reg.h:1737`), and
the nearest thing to a selector is `PIPE_MISC2`'s `FLIP_INFO_PLANE_SEL[2:0]`
at `0x7002C` (`i915_reg.h:1739-1746`), which is a different register.  The
read-modify-write mask is therefore right as it stands and should not be
"corrected" into a whole-register write.

## 4. Seven defects in the reference, and what this module does about them

Five of the seven were found by an adversarial review of this module against the
cached `[I915]` tree (§4.4 - §4.7) and three of those five were *followed* by the
first version of this module; the first two (§4.1, §4.2) were found while it was
written.  One (§4.3) is still open and does not change any value here.

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
invalid, hence the +1 here" (`display/skl_watermark.c:1992-1993`) — so 4095 is
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
display version 13 (`display/intel_display_device.c:1051`, `XE_LPD_FEATURES`'s
`ip.ver = 13`).

The generous level writes 31, which is legal under either reading, so nothing
in this workstream turns on the disagreement.  It matters for the real
watermark algorithm of §7.4, which would reject levels it need not reject if it
used the smaller bound.  Not resolved here; noted for whoever implements §7.4.

### 4.4 `PLANE_WM`'s level 6 and level 7 do not exist — they are the SAGV pair

**The defect the review found, and the one with the worst failure mode.**
§5.4 and §7.3 give the level formula `PLANE_WM(pipe, plane, level) = 0x70240 +
level*4`, and §7.4 step 1 has PCode return latency levels 0-3 and 4-7, so the
first version of this module iterated to eight and wrote a *disabled* level over
the formula's last two results.  The formula is right and the inference from it
is wrong:

* `0x70240 + 6*4 = 0x70258` is `_PLANE_WM_SAGV_1_A`
  (`skl_universal_plane_regs.h:327-333`);
* `0x70240 + 7*4 = 0x7025c` is `_PLANE_WM_SAGV_TRANS_1_A` (`:335-341`).

Two different registers, same field layout, different meaning.  Zeroing them
does not "disable levels 7 and 8": it disables the SAGV watermarks, and a
watermark whose `PLANE_WM_EN` is clear is a plane that reads nothing (§11 step
4.2, §7.1) — a black screen with correct timings, which is precisely the failure
this module's ordering exists to prevent.

`[I915]` programs **six** levels on this platform, not eight:
`skl_setup_wm_latency` sets `num_levels = 6` when `HAS_HW_SAGV_WM`
(`display/skl_watermark.c:3378-3383`), and `HAS_HW_SAGV_WM` is
`DISPLAY_VER >= 13 && !IS_DGFX` (`display/intel_display_device.h:141`) — ADL-N
is display version 13 and integrated, so it is six.  The level set here is 0..5
and the two offsets have constants of their own.

**The SAGV pair is programmed with level 0's generous value, not zero and not
disabled.**  `[I915]` programs them under `HAS_HW_SAGV_WM`
(`display/skl_universal_plane.c:743-748`) with the SAGV latency's own
watermarks.  This kernel never writes SAGV's control, so it cannot know which
set the hardware is using; the only value that is correct either way is the
generous one.  A SAGV watermark that is too generous underruns later and says so
in `PIPESTAT` (phase 6.4); one that is disabled shows nothing and says nothing.

### 4.5 `PLANE_WM_TRANS` has an offset, and the sequence writes it disabled

§5.6's `noarm` order names `PLANE_WM_TRANS` next to `PLANE_WM(0..n)` and no
section gives it an offset, so the first version of this module left it out and
said so.  The offset is in the same header the module already cites:
`_PLANE_WM_TRANS_1_A = 0x70268` (`skl_universal_plane_regs.h:343-349`), and
`[I915]` writes it immediately after the levels
(`display/skl_universal_plane.c:735-749`).

**It is written disabled, and that is `[I915]`'s value here rather than a
fallback.**  `skl_compute_transition_wm` computes the transition watermark from
level 0 as `wm0.blocks - 1 + trans_offset + 1` with `trans_offset = 14` on this
generation (`display/skl_watermark.c:2041-2100`), and then `skl_check_wm_level`
zeroes the whole level when its `min_ddb_alloc` is larger than the plane's DDB
allocation (`:1437-1441`).  With level 0 at the largest legal 4095 blocks inside
a 4096-block allocation those numbers are 4109 blocks and a `min_ddb_alloc` of
4110 — past the allocation and past `PLANE_WM_BLOCKS`' twelve-bit field — so the
register `[I915]` would write for this exact configuration is zero.  Writing
level 0's value there instead would be a value the vendor driver's own algorithm
rejects.

This is a transition watermark, not a level: `EN = 0` on it is what
`skl_check_wm_level` itself writes, and what `[I915]` leaves when IPC is
disabled (`skl_watermark.c:3238-3241`), not §7.1's "reads nothing".

### 4.6 `TRANS_VBLANK`'s low half is not `vdisplay − 1` on this display version

§6.1's formula says `VBLANK(T) = ((vtotal − 1) << 16) | (vdisplay − 1)`, and
§5.3 says all six timing registers store `value − 1` in both halves.  On display
version 13 the low half is a field the hardware no longer reads.  `[I915]`
(`display/intel_display.c:2717-2735`):

```c
	/*
	 * VBLANK_START no longer works on ADL+, instead we must use
	 * TRANS_SET_CONTEXT_LATENCY to configure the pipe vblank start.
	 */
	if (DISPLAY_VER(dev_priv) >= 13) {
		intel_de_write(dev_priv,
			       TRANS_SET_CONTEXT_LATENCY(dev_priv, cpu_transcoder),
			       crtc_vblank_start - crtc_vdisplay);
		/*
		 * VBLANK_START not used by hw, just clear it
		 * to make it stand out in register dumps.
		 */
		crtc_vblank_start = 1;
	}
```

after which `TRANS_VBLANK` is written as
`VBLANK_START(crtc_vblank_start - 1) | VBLANK_END(crtc_vblank_end - 1)`, whose
low half is therefore `0`.  The replacement register is
`_TRANS_A_SET_CONTEXT_LATENCY = 0x6007c` (`i915_reg.h:4027-4031`), the
transcoder base plus `0x7c`, with the same `+0x1000` stride the block uses
(`intel_display_device.c:73-77`).

So `timing.rs` now produces eight values, not seven: `TRANS_VBLANK`'s low half
is `0`, and `TRANS_SET_CONTEXT_LATENCY` carries `vblank_start − vdisplay` —
**undecorated**, not stored minus one.  In this kernel that difference is zero
for every mode in the tables, because vertical blanking starts at the active
height (`Mode::vblank()` is `vtotal - vdisplay`, `drm/modes/mode.rs:226-228`,
and the type has no other blanking-start field); it is still *written*, because
a register this bring-up reads must not keep a reset value.

**Not verified:** the source that fills DRM's `crtc_vblank_start` is
`drm_mode_set_crtcinfo`, which is not in the cached tree.  "The difference is
zero" is argued from this kernel's own mode type and from i915's second use of
the same quantity (`display/intel_alpm.c:296-298`), not from reading DRM's
assignment.  If a mode with a different blanking start is ever added, this
register and `VBLANK`'s low half are the two places that must change with it.

### 4.7 Three bits `[I915]` sets for display version 13 and the reference omits

All three are one workaround and two output-quality bits that the reference
document either does not mention or lists without a value:

* **`PLANE_CTL`'s `ARB_SLOTS[30:28]` and `PIPE_ARB_CTL`'s `USE_PROG_SLOTS[13]`
  are one workaround, `Wa_22012358565:adl-p`, and both halves are
  `DISPLAY_VER == 13` only.**  `skl_plane_ctl` ORs
  `adlp_plane_ctl_arb_slots(plane_state)` into `plane_ctl` under that version
  (`display/skl_universal_plane.c:1090-1092`), and that function returns
  `PLANE_CTL_ARB_SLOTS(1)` for a plane that is not YUV semi-planar and whose
  first component is four bytes wide (`:1014-1035`, the `case 4` at `:1029-1030`)
  — an XRGB8888 surface exactly.  The pipe half is
  `intel_de_rmw(dev_priv, PIPE_ARB_CTL(pipe), 0, PIPE_ARB_USE_PROG_SLOTS)` in
  `intel_enable_transcoder` (`display/intel_display.c:441-445`).  The field is
  `REG_GENMASK(30, 28)` (`skl_universal_plane_regs.h:39-40`), so `ARB_SLOTS(1)`
  is `1 << 28`; section 5.2 gives `PIPE_ARB_CTL`'s offset (`0x70028`) and field
  already, and `[I915]` agrees with it (`i915_reg.h:1704-1706`).  A pipe using
  programmed slots and a plane whose `ARB_SLOTS` says the wrong thing is an
  arbiter that under-serves the plane — an underrun that phase 6.4 would report
  as a watermark problem.
* **`PLANE_COLOR_CTL`'s `PLANE_COLOR_PLANE_GAMMA_DISABLE[13]`.**  §11 step 4.3
  says "alpha disabled, no CSC" and the first version of this module wrote zero,
  which *enables* the plane's gamma correction with no gamma table programmed.
  `glk_plane_color_ctl` ORs this bit into the register for every plane it builds
  (`display/skl_universal_plane.c:1114-1124`, the OR at `:1123`), field
  `REG_BIT(13)` at `skl_universal_plane_regs.h:262`.  This is a gamma *disable*,
  not the gamma block: no gamma table is programmed here.
* **`PIPE_MISC`'s `PIXEL_ROUNDING_TRUNC[8]`.**  `bdw_set_pipe_misc` sets it for
  `DISPLAY_VER >= 12` (`display/intel_display.c:3289-3290`), field `REG_BIT(8)`
  at `i915_reg.h:1719` (commented `tgl+`).  The reference's §5.2 lists the bit
  and never says what to do with it; the read-modify-write now owns it and sets
  it (§3.3).

**Worth flagging separately:** the reference calls this platform "Gen12 /
Xe-LP" throughout.  In i915's terms ADL-N is `XE_LPD` with display IP version
13, not 12, and version-gated behaviour is a real trap on this platform — §15's
"meta-lesson" says exactly that.  `PIPE_MISC`'s BPC field changing meaning at
version 13 ("For Display < 13, Bits 5-7 represent DITHER BPC ... ADLP+, the
bits 5-7 represent PORT OUTPUT BPC", `i915_reg.h:1720-1725`) is one example
this module depends on, the 12-bit DDB fields of §7.2 are another, and §4.4 and
§4.7 are two more: `PLANE_WM`'s level count and both halves of
`Wa_22012358565` are all version-13-only facts.

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
  published totals** (`0x0897_077f`, `0x0803_07d7`, `0x0464_0000`,
  `0x0440_043b`, `0x077f_0437`), which is arithmetic, not a measurement.  The
  one value that is *not* arithmetic — `TRANS_SET_CONTEXT_LATENCY`'s zero — is
  argued from this kernel's mode type rather than from DRM's assignment of
  `crtc_vblank_start` (§4.6).
* **No plane has been observed to arm.**  `PLANE_SURFLIVE` matching what was
  written is what §11 step 6.2 asks for, and on the mock it is a `set()`.  On
  hardware a zero after the poll means the plane has not latched — the arm is
  latched at a vblank, so read the phase-6.1 verdict before concluding anything
  about the address — and a *different* non-zero address most likely means the
  firmware's surface is still live (or that the arm did not take).
* **Whether a pre-enable `PLANE_SURF` would latch later is not known.**  §2's
  arm step exists so that nothing here depends on the answer (§4.6's ordering
  note has the citations and the honest limit).
* **No underrun has been observed.**  Nothing here establishes that 4095 blocks
  and 31 lines are enough for a 1080p60 plane on this machine's memory system.
  §7.3 predicts the opposite: the generous level "over-allocates and may
  under-run on a busy memory system".
* **The SAGV pair's generous value is a choice, not a measurement.**  This
  kernel cannot read SAGV's state, so "level 0's value is correct whether or not
  SAGV is active" is an argument from §7.1's failure mode, not from a
  measurement of what the hardware does at each memory frequency (§4.4).
* **The line rate the phase-6.1 check reports has never been compared with a
  real PLL.**  On the mock the samples are a counter; on hardware the number is
  the check on §12.3's "only way to verify your PLL arithmetic against reality
  without a scope", and the first boot is where it becomes evidence.
* **`PIPESTAT` bit 31 has never been read from a real register.**  See §7.
* **`PLANE_STRIDE`'s 64-byte encoding is a source reading**, not a measurement
  (§4.1).
* **The 4 KiB alignment and 64-byte stride rules are enforced but not
  exercised against a real allocation.**  WS-1's framebuffer must satisfy them;
  this module only refuses them.

## 7. Values I could not source

* `PLANE_KEYVAL`, `PLANE_KEYMSK`, `PLANE_KEYMAX`, `PLANE_AUX_DIST`,
  `PLANE_AUX_OFFSET`: named by §5.6, no offset anywhere in the reference.  Not
  written, not declared.  (`PLANE_WM_TRANS` was in this list and is not any
  more: `[I915]` has its offset, and §4.5 records what is written there.)
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
* `TRANS_SET_CONTEXT_LATENCY`'s value and DRM's `crtc_vblank_start`: the
  register is `[I915]`'s and the difference is this kernel's own mode type's,
  not DRM's assignment, which is not in the cached tree (§4.6).
* `PLANE_WM_SAGV`'s and `PLANE_WM_SAGV_TRANS`' correct values under SAGV: the
  generous level-0 value is written because this kernel cannot read SAGV's state
  (§4.4), not because the number was computed for a memory frequency.

## 8. A gap that belongs to the surface, not to this module

WS-1 reported, and this workstream confirms by reading the same registers, that
the GGTT page-table write cannot be followed by a TLB invalidate in this
kernel: `[I915]` writes `GEN12_GUC_TLB_INV_CR` (`0xcee8`, bit 0) after a PTE
update for graphics version 12 and later (`gt/intel_ggtt.c:237-252`,
`guc_ggtt_invalidate`, with the register write at `:249-250`), that offset lies
inside `FORCEWAKE_GT` per §2.1, and
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

`cargo test` for `drm::intel` on the host, with the command the brief fixes:
**274 passed, 0 failed, 0 ignored** (1955 filtered out by the `drm::intel`
name filter), of which 36 are this module's and 17 are `timing.rs`'s.  The
interesting ones assert properties rather than return values:

| test | property |
|---|---|
| `the_write_order_is_phase_3_4_then_4_1_then_4_2_then_4_3` | the exact 25-write shadow sequence |
| `the_arm_pair_is_adjacent_and_last` | `PLANE_CTL` then `PLANE_SURF` as a separate pair, adjacent, last, and in neither the shadow list nor the shadow log |
| `a_refused_plane_control_write_in_the_arm_leaves_the_plane_unarmed` | a refused `PLANE_CTL` in the arm means no `PLANE_SURF` at all |
| `computing_the_write_list_writes_nothing` | the whole list is computed before the first write, the capacity reserved is the count, and `program` performs exactly it |
| `the_plane_is_armed_only_after_the_watermark_enable_bit` | `PLANE_WM_EN` is set in the write that precedes the commit |
| `a_refused_watermark_write_leaves_the_plane_unarmed` | a refused write stops the sequence and no `PLANE_SURF` is ever written |
| `the_watermark_offsets_above_level_five_are_not_written_as_levels` | `0x70258`/`0x7025c` carry the SAGV pair with `EN` set, and `PLANE_WM_TRANS` is written disabled (§4.4, §4.5) |
| `the_timing_registers_are_exactly_what_timing_produced` | the written values are `timing.rs`'s, checked against VIC 16's published totals and the cleared `VBLANK_START` |
| `the_plane_size_is_the_source_size_with_its_halves_swapped` | `PLANE_SIZE` decodes to (1080, 1920), `PIPESRC` to (1920, 1080) |
| `the_stride_is_written_in_sixty_four_byte_units` | 7680 bytes → 120 |
| `pipe_arb_ctl_sets_the_programmed_slots_bit_and_nothing_else` | every bit but `USE_PROG_SLOTS` survives the read-modify-write |
| `pipe_misc_keeps_the_bits_this_bring_up_does_not_own` | a firmware `HDR_MODE_PRECISION` survives, and `PIXEL_ROUNDING_TRUNC` is *set* rather than preserved |
| `each_pipe_has_its_own_registers` | every pipe's registers are at §5.2/§5.4/§5.3's stated stride, including the SAGV pair and `TRANS_SET_CONTEXT_LATENCY` |
| `an_underrun_names_the_watermarks_and_the_ddb` | the verdict carries the `PLANE_WM` and `PLANE_BUF_CFG` values and points at §7.1 |
| `a_pipe_that_is_not_scanning_is_a_named_failure` | `PIPEDSL` that never changes is named, the rate is reported as `none`, and the other two checks still run |
| `a_repeated_scanline_sample_is_not_a_stopped_pipe` | four samples compared pairwise on their *lines*, so one frame's coincidence is not a stopped pipe |
| `the_verdict_reports_the_rate_the_samples_measured` | §12.3's line rate is reported from the samples, and a wrapped interval is excluded rather than averaged in |
| `a_late_latch_is_armed_rather_than_reported_missing` | phase 6.2 polls, so an arm that latches after the first read is `Armed` |
| `a_plane_that_did_not_latch_is_a_named_failure` | waiting out the deadline with `SURFLIVE == 0` is "not latched yet", a *different* address is its own verdict, and neither message claims the address was rejected |
| `a_clock_that_never_advances_does_not_hang_the_check` | the pause budget bounds a misbehaving timer in both polls |
| `the_vblank_start_field_is_cleared_and_the_context_latency_replaces_it` | `timing.rs`'s ADL+ substitution, with literals (§4.6) |

The refusal of a 7681-byte stride — the pitch that is one byte too wide for
`PLANE_STRIDE` — is asserted by `a_surface_that_cannot_be_scanned_out_is_refused`,
which is the test that walks every phase-3.2 rule; the stride test above covers
only the encoding.  An earlier version of this table credited the refusal to the
stride test, which does not contain it.

The limits of that: a mock cannot fail to arm a plane for a reason the mock was
not told about, `PIPEDSL` in a mock is a counter and not a scanline, and an
underrun is a bit this test sets by hand.  **Every "passes" above means "the
sequence does what it says", never "the hardware works".**

## 10. What a first run on the machine should do with this

1. Log `PipeProgram::render(pipe_misc_before, arb_ctl_before)` before
   programming, so the boot log carries the mode, the 25 shadow values, the arm
   pair and the DDB/watermark numbers even if the screen stays black.
2. `program` (the shadow half), then the output workstream's phase 5 — which is
   what writes `TRANSCONF` and starts the pipe — then `arm`, then
   `prove_at_boot`.  Not the other order: the arm latches at a vblank and a
   disabled pipe has none (§2).
3. Read the three verdicts in the order they are produced, because they are
   ordered by how much they tell you: no `PIPEDSL` change means the pipe is not
   scanning and nothing later matters; a `PLANE_SURFLIVE` that is still zero
   after the poll means the plane has not latched — check the phase-6.1 verdict
   before blaming the address — while a *different* address means the arm did
   not take; a set bit 31 means the watermarks or the DDB are wrong, which is
   §11 step 6.4's "go back to 4.2 before changing anything else".  The phase-6.1
   line also carries the observed line rate, which is §12.3's check on the PLL:
   67 000 lines/s for 1080p60 at 148.5 MHz over a 2200-pixel line.
4. §11 step 6.5's advice is the other half of this workstream's usefulness and
   belongs to whoever fills the framebuffer: **fill it with vertical colour
   bars, not black.**  A solid black surface is indistinguishable from every
   failure listed above.
