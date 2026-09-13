# The Intel swing read-back: getting phase 5's buffer-translation values off the PHY

**What this covers:** reference §8.5's voltage-swing / buffer-translation values — the numbers phase 5
writes into the combo PHY's `PORT_TX_DW2`/`DW4`/`DW5`/`DW7` — and the one honest route to them:
reading back what the firmware already programmed for this board. §8.3 (combo PHY initialisation),
§8.4 (`DDI_BUF_CTL`'s fields), §8.6 steps 7 and 13-14, §11 phase 5.3 and §13.4 are the sections this
route is built on.

**Code:** `kernel/src/drm/intel/swing.rs` and its tests in `kernel/src/drm/intel/swing/tests.rs`.
**Reference:** `docs/design/intel-display-registers.md` §8.1–§8.6, §11 phase 5, §12.2, §13.1 item 12,
§13.4; `docs/design/intel-output.md` for the sequence that consumes the result.

**Status: nothing in this module has run against silicon.** Every register in it has been read only
from a host mock; no Gen12 display engine has answered any of these reads. The register state the
tests use is *invented*, shaped like a program but not claimed to be one. The consistency checks are
the ones the document and i915 justify; whether the firmware on the Acer 蜂鸟mini leaves that state is
exactly what has not been observed.

## 1. The gap, and why the read route is the honest one

Phase 5 cannot program a DDI buffer without a `SwingProgram`: the values are the *board's* voltage
swing and de-emphasis, and §8.5 gives them for DisplayPort while marking the HDMI table
(`icl_combo_phy_trans_hdmi`, the table §8.5 selects for an HDMI port) as `[GAP]` — *"I did not
extract its values"*; §13.1 item 12 repeats it. §8.5 records why the numbers cannot be derived from
the PHY IP: the DG1 PRM's table and i915's ADL-P table disagree for the *same nominal level*, because
they are tuned to the package and board parasitics of different machines. Neither is a fact about
this Acer.

Three routes existed:

1. **Transcribe i915's `icl_combo_phy_trans_hdmi`.** It is a real table for a real ADL-P/N part, and
   it is the table i915 would use on this machine. But it was measured on Intel's reference boards,
   not on this one; §13.3 trusts it over the DG1 PRM precisely because it is the platform's table,
   which is a statement about *which* board's numbers are closer, not a measurement of *this* board.
   `[INF]` It would very probably work at 1080p60. It is a decision for the user to make, and this
   code does not make it: nobody has measured this connector.
2. **Guess the values.** Excluded by the project's own rule and by §8.5.
3. **Read them out of the PHY the firmware programmed.** §13.4: *"Boot the machine with a vendor
   driver (or just the firmware's own GOP), let it modeset successfully, then dump the display
   register window before anything clears it. A 2 MiB dump from a working configuration is worth
   more than every table in this document."*

Route 3 is what this module implements, narrowed to the registers phase 5 needs. On the target
machine the screen is the only console: the firmware (or the bootloader's GOP) has already brought a
mode up, and the PHY's TX registers hold the values that drive *this* connector on *this* board. A
read of them is a dump of a working configuration — the same artefact §13.4 asks for first.

`[INF]` Two things this route assumes and does not prove: that the state the kernel starts in is the
firmware's own successful modeset (not a half-torn-down state), and that the firmware programmed the
combo PHY TX registers rather than the indexed `DDI_BUF_TRANS_LO/HI` pair §8.4 also declares. Both
are settled by the same dump: §12's register snapshot plus a diff against a snapshot taken with no
monitor attached.

## 2. Provenance

The reference's markers are used unchanged.

| Marker | Meaning |
|---|---|
| `[REF]` | `docs/design/intel-display-registers.md`, by section. |
| `[I915]` | Linux v6.12 `drm/i915` (GPL-2.0), read as facts and cited by file and line. No code or prose was copied. |
| `[MEASURED]` | Produced by a test in this repository, on the host. Not a hardware result. |
| `[INF]` | Inference from sourced facts. Labelled, never presented as sourced. |
| `[GAP]` | Could not be sourced. Listed again in §6. |

## 3. The registers, and the bits taken from each

For a `Ddi` on a combo PHY — DDI A on PHY A (`0x162000`), DDI B on PHY B (`0x06C000`), §8.1 and §8.2
— the module reads, all through the register table's declarations (`regs/port.rs`, `regs/mod.rs`):

| Register | Offset (A / B) | What is taken | Source |
|---|---|---|---|
| `PORT_COMP_DW0` | `0x162100` / `0x06C100` | bit 31 `COMP_INIT` | §8.3 steps 2, 6; §12.2; `[I915]` `display/intel_combo_phy_regs.h:55` |
| `DDI_BUF_CTL` | `0x64000` / `0x64100` | bit 31 `ENABLE`, bit 7 `IS_IDLE`, `[27:24]` `BUF_TRANS_SELECT` | §8.4; §8.6 steps 13-14; §11 phases 5.7, 6.3; `[I915]` `i915_reg.h:3859`, `:3861-3862`, `:3869` |
| `PORT_CL_DW10` | `0x162028` / `0x06C028` | `[7:4]` `PWR_DOWN_LN_MASK` | §8.2; §8.6 step 7; `[I915]` `display/intel_combo_phy_regs.h:34-37` |
| `PORT_TX_DW2_LN0..3` | `0x162888 + 0x100·ln` / `0x06C888 + 0x100·ln` | the whole dword, **per lane** | §8.2 rule `0x880 + ln·0x100` (`DW2` adds `0x8`); §8.5 step 5; `[I915]` `display/intel_ddi.c:1148-1157` |
| `PORT_TX_DW4_LN0..3` | `0x162890 + 0x100·ln` / `0x06C890 + 0x100·ln` | the whole dword, **per lane** | §8.2 rule `0x880 + ln·0x100`; §8.5 steps 2 and 5; `[I915]` `display/intel_ddi.c:1159-1169` |
| `PORT_TX_DW5_LN0` | `0x162894` / `0x06C894` | bit 31 `TX_TRAINING_EN`, and the word itself | §8.5 steps 4-6; `[I915]` `display/intel_combo_phy_regs.h:133`, `display/intel_ddi.c:1141`, `:1218`, `:1226` |
| `PORT_TX_DW7_LN0..3` | `0x16289C + 0x100·ln` / `0x06C89C + 0x100·ln` | the whole dword, **per lane** | §8.2 rule (`DW7` adds `0x1c`); §8.5 step 5; `[I915]` `display/intel_ddi.c:1171-1178` |

Two choices about *how* they are read matter:

* **`PORT_TX_DW2`, `PORT_TX_DW4` and `PORT_TX_DW7` are read one lane at a time, never as a
  group.** For `DW4` §8.5 step 2 says the loadgen select differs per lane and the paragraph
  after the sequence says group access must not be used for it; i915's comment on the same
  write is *"We cannot write to GRP. It would overwrite individual loadgen"*
  (`[I915]` `display/intel_ddi.c:1159-1160`). For `DW2` and `DW7` the document's per-lane
  sentence names `DW4` only, so the evidence is i915's sequence, which writes both of them
  per lane over `ln = 0..3` (`[I915]` `display/intel_ddi.c:1148-1157`, `:1171-1178`).
  §8.2 defines three instances of each TX dword — AUX `+0x380`, group `+0x680`, lane
  `0x880 + ln·0x100` (`[I915]` `display/intel_combo_phy_regs.h:96-105`) — and they are three
  addresses, so a group read is not a read of what a per-lane write left.
* **`PORT_TX_DW5` is read from lane 0.** That is the copy i915 reads before each of the two
  writes it makes to the group instance: `ICL_PORT_TX_DW5_LN(0, phy)` for the read,
  `ICL_PORT_TX_DW5_GRP(phy)` for the write (`[I915]` `display/intel_ddi.c:1218-1229`, and the
  same pair inside the batch at `:1141-1146`). `output::program` writes the group instance for
  the same two writes. The group instances of `DW2`/`DW7` stay declared in `regs/port.rs`
  because §8.2's worked examples tabulate them; a DDI's swing does not land in them.
* **Every dword is read whole and replayed whole.** §8.5 enumerates the table's fields but not
  the registers' other bits, and i915 writes fields the document never names — `RCOMP_SCALAR(0x98)`
  in `DW2`, `RTERM_SELECT(0x6)` and `TAP3_DISABLE` in `DW5`, the loadgen bit in `DW4`
  (`[I915]` `display/intel_ddi.c:1141-1178`). A masked read would drop them; a whole
  read is the exact inverse of the whole write `output::program` performs.

## 4. The evidence required before the values are believed

A register read is not evidence that a register holds a program. A port the firmware never brought up
still answers reads — with zero, with a reset value, or with values from an earlier mode. So
`read_firmware_swing` refuses unless all of the following hold, each as its own named `SwingSource`:

| Evidence | Refusal if absent | Why this is the evidence |
|---|---|---|
| `PORT_COMP_DW0.COMP_INIT` set | `PhyNotInitialised` | §8.3 steps 2 and 6: the PHY's own record that initialisation ran; §12.2 reads the same bit to decide whether a PHY exists and is initialised, and `phy::init_one` treats it as "already initialised". |
| `DDI_BUF_CTL.ENABLE` set | `PortNotEnabled` | §8.6 step 13 and §11 phase 5.7: the port's buffer is on. |
| `DDI_BUF_CTL.IS_IDLE` clear | `PortStillIdle` | §11 phase 5.7 calls `IS_IDLE` the single best "is my DDI alive" bit on the chip; phase 6.3 makes it the read-back that proves the port is scanning. An idle port has no working configuration to copy. |
| at least one lane powered (`PWR_DOWN_LN_MASK` ≠ `0xf`) | `LanesAllPoweredDown` | §8.6 step 7 powers the lanes *before* step 13 enables the buffer. A buffer that says "enabled, not idle" while every lane is down is not a state that sequence produces. |
| not every swing word all-zero or all-ones (both per-lane dwords, and lane 0 of `DW5`) | `PhyNotResponding` | §12.2: an absent block answers all-zero or all-ones. |
| `PORT_TX_DW5_LN0.TX_TRAINING_EN` set | `TrainingNotEnabled` | §8.5 step 6 and `[I915]` `display/intel_ddi.c:1226-1229`: setting that bit is the write that triggers the update. With it clear the batch was never committed — and it is the bit the derived state is built from. |
| the DDI is A or B | `UnsupportedDdi` | §8.1: C and D are Type-C/DKL ports; the register table declares no TX registers for them. Refused before the first read. |
| every register readable | `Unreadable` | §2.2: a register that could not be read is not a register that read zero. |

Two checks that would only describe *how* the firmware got there are deliberately **not** refusals:

* `PORT_TX_DW5`'s scaling-mode field (`[20:18]`; §8.5 step 5 says `0b010`, i915 writes
  `SCALING_MODE_SEL(0x2)` — `[I915]` `display/intel_combo_phy_regs.h:136-137`,
  `display/intel_ddi.c:1143`). The whole dword is replayed, so a different value changes nothing
  about the values' honesty, while refusing on it could block a board whose firmware used a
  different-but-working setting.
* whether `PWR_DOWN_LN_MASK` agrees lane-for-lane with `DDI_BUF_CTL`'s width (§8.6 step 7's three
  states are `0x0`/`0xC`/`0xE` for 4/2/1 lanes). Same argument: both are replayed fields, and a
  disagreement does not make the swing values wrong.

## 5. The two `PORT_TX_DW5` states, and how the second is derived

`SwingProgram` carries `dw5_training_disabled` and `dw5_training_enabled` because §8.5's sequence
writes two states: training disabled while the table values land (step 4), then training enabled as
the write that commits them (step 6). The firmware's register holds only the second.

**The first is derivable, and this module derives it.** `TX_TRAINING_EN` is bit 31
(`[I915]` `display/intel_combo_phy_regs.h:133`), and i915's sequence is exactly: read `DW5`, clear
that bit, write it, program `DW2`/`DW4`/`DW7`, read `DW5`, set the bit, write it
(`[I915]` `display/intel_ddi.c:1218-1229`). Between the two writes the only change to the register is
the scaling-mode/rterm/tap batch, so the state that was live *while the table values were written* is
the read state with bit 31 cleared — nothing else. `dw5_training_disabled = read & !TX_TRAINING_EN`,
and every other field is carried through untouched.

`[REF]` §8.5 states the same two states as steps 4 and 6 but gives neither field's bit position, which
is why `docs/design/intel-output.md` §5 item 2 recorded them as caller-supplied dwords. `[I915]`
supplies the position; that item is now closable in this direction, and this module closes it for the
read route rather than changing `SwingProgram`'s shape. A caller that has both states from a dump can
still supply them literally.

## 6. What is still unverified, and what would close it

1. **The whole route, on silicon.** No register here has been read from a Gen12 display engine. The
   first real read is the test: if the firmware's state does not match §4's evidence, the refusal
   names which bit disagreed, and that log line is itself the finding.
2. **`BUF_TRANS_SELECT` as the statement of the level.** §8.6 step 13 writes the level there and §8.4
   makes it the table index, which is why the level is read from it. But `[I915]`'s
   `has_buf_trans_select()` is `DISPLAY_VER < 10` (`display/intel_ddi.c:106-109`), so i915 on Gen12
   selects the level by programming the PHY and **never writes this field**; a firmware that does the
   same leaves it at its reset value 0. The read returns whatever the field says — it cannot
   distinguish "level 0" from "nobody wrote it", and it should not pretend to. On Gen12 the field is
   an index the hardware may not consult, so a wrong value here costs a log line rather than a
   picture; a dump settles it.
3. **Group vs per-lane `DW2`/`DW7` — closed.** §8.5's step 5 names the two dwords without
   naming one of the three instances §8.2 defines, and its per-lane carve-out is stated for
   `DW4` only, which is why this module originally read the group instances. i915's DDI
   voltage-swing sequence answers it: `DW2` and `DW7` are written per lane over `ln = 0..3`
   (`[I915]` `display/intel_ddi.c:1148-1157`, `:1171-1178`) and `DW5` is read from lane 0
   before the group instance is written (`:1141-1146`, `:1218-1229`). `regs/port.rs` now
   declares `PORT_TX_DW2_LN0..3`, `PORT_TX_DW7_LN0..3` and `PORT_TX_DW5_LN0` beside the group
   instances, `read_firmware_swing` reads the lane instances, and `output::program` writes
   them. What remains open is not the shape but what *this machine's* firmware left: the dump
   in §6 item 1 is still the first measurement, and it now compares four lane addresses per
   dword instead of one group address. An `[INF]` corollary worth stating: i915's own step 6
   reads lane 0 after its step-4 group write, so it relies on a group write being visible at
   lane 0; the reverse — a per-lane write showing up in the group instance — is what the old
   read assumed and is the assumption this change removes.
4. **Whether the firmware's final `DW5` has `TX_TRAINING_EN` set.** §8.5 step 6 and i915 both leave
   it set; a firmware that clears it after the update would be refused (`TrainingNotEnabled`) even
   though its swing values are good. The refusal text says exactly what was read, so the log
   distinguishes this from a port that was never programmed.
5. **The indexed `DDI_BUF_TRANS_LO/HI` pair.** §8.4 declares it, §8.5's sequence does not write it.
   If the firmware programmed *that* instead of the PHY TX registers, the read route has to move; the
   same dump (read both register sets in a working mode) decides it.
6. **The `[GAP]` itself, if the read route turns out to be unusable.** Transcribing
   `icl_combo_phy_trans_hdmi` is a decision for the user: it is another board's calibration, it is
   `[I915]`'s own table for this platform, and nothing in this repository will do it silently. If it
   is ever taken, it belongs beside this module as an explicit, cited alternative that a caller
   chooses, never as a fallback that a failed read falls into.

## 7. How phase 5 gets it

`modeset.rs` — the boot path that will call phase 5 — is another workstream's and is not merged, so
nothing calls this module yet. The composition is:

```text
match swing::read_firmware_swing(&window, ddi) {
    Ok(swing) => swing,
    Err(refusal) => {
        log!("intel: DDI {}: {} -- not mode-setting", ddi.name(), refusal);
        return;                        // do not attempt the modeset
    }
}
let request = output::OutputRequest::hdmi(ddi, mode, encoding).with_swing(swing);
let program = output::OutputProgram::plan(&request, platform_ref_khz)?;
output::program(&window, &program)?;
```

**There is no fallback.** A refusal means this kernel could not obtain this board's swing values; the
only alternatives are i915's board-tuned table (the user's decision, §6 item 6) or a screen that stays
dark. It logs and does not program the output. A caller that caught the refusal and supplied invented
numbers would be doing exactly what §8.5's `[GAP]` exists to prevent.

## 8. What the tests measure

`[MEASURED]` 283 `drm::intel` host tests pass, 15 of them this module's, none failing. The 15
assert:

* a register state shaped like a program reads back whole: `level`, the per-lane `dw2`, the
  per-lane `dw4`, both `DW5` states, the per-lane `dw7`, and a `source` string naming the DDI
  and the level;
* the level comes from `DDI_BUF_CTL.BUF_TRANS_SELECT` — changing that field moves `level` and
  `source` and leaves the swing words untouched;
* `dw5_training_disabled` is the read value with bit 31 cleared and every other field preserved;
* four different lane values come back from four different registers for each of `DW2`, `DW4` and
  `DW7`, at the `0x100·ln` lane stride and nowhere near the group instance;
* `DW5` comes from lane 0 and not from the group instance, in both directions: a mock whose two
  instances differ returns lane 0's word, and a port whose group holds the training bit while lane
  0 does not is refused;
* DDI A and DDI B read sixteen *different* addresses, and two different programs in one aperture
  come back as two different `SwingProgram`s (the aliasing test: a shared mapping would set port
  B's swing from port A's calibration);
* one degenerate lane is not an absent block — §12.2's answer is every instance — while an
  untouched aperture is refused as `PhyNotInitialised`, an initialised PHY with no enabled buffer
  as `PortNotEnabled`, a still-idle port as `PortStillIdle`, all-lanes-down as
  `LanesAllPoweredDown`, a `DW5` without the training bit as `TrainingNotEnabled`, a dead block as
  `PhyNotResponding` (both all-zero and all-ones), a hidden register as `Unreadable`, and Type-C DDIs
  as `UnsupportedDdi`;
* every refusal's `describe()` names its DDI and its register, and `Display` agrees with it;
* the composition works end to end on a mock: `OutputRequest::hdmi` without swing values is refused
  with `MissingBufferTranslation` — the `[GAP]` doing its job — and with the read-back program the
  same request plans, carrying the level and the dwords that were read.

What the tests cannot measure is in §6 item 1: none of this has run on hardware, and the register
state they use is invented.
