# The Intel output path: phase 5 of the bring-up order

**What this covers:** reference section 11 phase 5 — the PLL that makes the port clock, the mapping
from a DDI to that PLL, the lane power and buffer translation, `TRANS_CLK_SEL`,
`TRANS_DDI_FUNC_CTL`, `TRANSCONF`, `DDI_BUF_CTL` and the `IS_IDLE` poll. Phases 1 to 4 are the
other workstreams'; this is the part that puts a signal on the wire.

**Code:** `kernel/src/drm/intel/output.rs` and its tests in `kernel/src/drm/intel/output/tests.rs`.
**Reference:** `docs/design/intel-display-registers.md` §6.3, §8.2, §8.4, §8.5, §8.6 and §11 phase 5;
`docs/design/intel-pll.md` for the arithmetic this module calls.

**Status: nothing in this module has run against silicon.** Every number below was produced on the
host, by a test over `regs::mock::MockRegisters` or by reading the reference. No register value has
been written to or read from a Gen12 display engine. The mock proves the *sequence* — which
registers, in what order, how many times, and what happens when a status bit never appears — and it
says nothing at all about whether the hardware accepts any of it.

## 1. Why this is a separate module

`pll.rs` and `timing.rs` are pure functions: a mode in, register values out, no MMIO. This module is
the first one that has to *touch* the device for the output half, and it keeps the same seam: a
caller builds an `OutputProgram` with `OutputProgram::plan`, which takes no register file, and
then hands it to `output::program`, which does the writes. The split is not tidiness. §11 phase 5.1
asks for `ref`, `(P, Q, K)` and the resulting symbol rate to be **printed before writing**, because a
wrong clock is a monitor that says "out of range" and a bug nobody can find; `OutputProgram::render`
is that print, and a failure part-way through the sequence still leaves the whole program in the log.

The order inside `program` is §8.6's enable sequence, steps 3 to 14, restricted to the output half
(the timing registers, the plane and the watermarks belong to the pipe workstream and happen between
step 8 and step 11):

```
PLL power on -> poll POWER_STATE -> CFGCR0/CFGCR1 -> posting read -> PLL enable -> poll LOCK
ICL_DPCLKA_CFGCR0: DDI_CLK_SEL, then DDI_CLK_OFF cleared in a separate write
DDI-IO power well -> poll STATE
PORT_CL_DW5 SUS_CLOCK_CONFIG, the swing writes, PORT_CL_DW10 lane power
TRANS_CLK_SEL(A), TRANS_DDI_FUNC_CTL(A), TRANSCONF(A)
DDI_BUF_CTL(A), then poll IS_IDLE == 0
```

## 2. Provenance

The reference's markers are used unchanged.

| Marker | Meaning |
|---|---|
| `[REF]` | `docs/design/intel-display-registers.md`, by section. |
| `[I915]` | Linux v6.12 `drm/i915`, as cited by `[REF]`. Nothing here was read from i915 directly. |
| `[MEASURED]` | Produced by a test in this repository, on the host. Not a hardware result. |
| `[INF]` | Inference from sourced facts. Labelled, never presented as sourced. |
| `[GAP]` | Could not be sourced. Listed again in §5. |

## 3. What the sequence computes, and where each value comes from

### 3.1 The PLL (5.1)

`pll.rs`'s `ddi_pll_dividers` is called with the mode's pixel clock — for HDMI and DVI the TMDS
character rate *is* the pixel clock, which is the case §6.3 works through — the platform reference
and the combo PHY. Everything else is `pll.rs`'s: the `(P, Q, K)` search, the 38.4 → 19.2 MHz
reference division of §6.3 fact 1, and the ADL-P/N fraction halving of §6.3 fact 2, which this
module selects with `DcoFractionWorkaround::for_adl_p_n(platform_ref_khz)`.

The platform reference is read from `SKL_DSSM[31:29]` by `read_platform_reference_khz`, which is the
first thing §11 phase 5.1 asks for; the decode is `pll.rs`'s, because the same three values select
the arithmetic there.

`[MEASURED]` For CTA-861 VIC 16 (1920×1080@60, 148.5 MHz) at a 38.4 MHz strap, the module computes
exactly what §6.3's worked example prints:

| Quantity | Value |
|---|---|
| `(P, Q, K)` | `(2, 3, 2)`, total divider 12 |
| DCO | 8 910 000 kHz, aimed at the 9 000 000 kHz centre, 1.00% away |
| reference into the arithmetic | 19 200 kHz |
| `DCO_FRACTION` before / after the workaround | 2048 / 1024 |
| `DPLL0_CFGCR0` | `0x001001D0` |
| `DPLL0_CFGCR1` under the Gen12 named-constant encoding | `0x00000E84` |
| achieved symbol rate | 148 500 kHz, 0 ppb from the request |

That test would fail if `pll.rs`'s arithmetic, the reference division, the workaround or the field
encoding were wrong, so it is the strongest sourced check this module has. It is still a host test.

### 3.2 The encoding question is the caller's

`PllFieldEncoding` has no default in `pll.rs` and none here: `OutputRequest.encoding` must be
named. `[REF]` §6.3's section "The `PDIV`/`KDIV` encoding — resolved" concludes that on the ADL-N
path write and read agree on the Gen12 named-constant values, and its worked example uses them, so a
caller that wants the reference's own numbers passes `PllFieldEncoding::Named`. The other encoding
`pll.rs` offers — the Skylake `CFGCR2` codes that `skl_wrpll_params_populate` writes — gives
`DPLL0_CFGCR1 = 0x00000E44` for the same divider set (`[MEASURED]`, pinned in
`the_two_encodings_disagree_on_the_same_divider_set`), because `K = 2` is coded `2 << 6` by the
named constants and `1 << 6` by the Skylake ones.

`[REF]` §13.1 item 7 still lists the encoding as a `[GAP]`; §6.3 and §13.3 both record it as
resolved, and §13.3's own row says "resolved". `pll.rs`'s module documentation was written against
the earlier draft of §6.3 that had it open, so it still frames it that way. **The document
contradicts itself and item 7 is the stale half.** This module does not resolve the contradiction by
silence: the caller names the encoding, the log prints which one was used, and the two hex values
above are pinned in a test.

### 3.3 The DDI-to-PLL mapping (5.2)

`ICL_DPCLKA_CFGCR0` gets `DDI_CLK_SEL(pll_id, phy)` in one write and the matching `DDI_CLK_OFF` bit
cleared in a second. §6.3 quotes the spec through `_icl_ddi_enable_clock`: the two *"must be done
with separate register writes"*. A test asserts `write_count(ICL_DPCLKA_CFGCR0) == 2`, that the
first write keeps the gate bit and sets the select, and that the second clears the gate and keeps
the select — a merged read-modify-write would pass a test that only checked the final value, so the
test checks the log.

### 3.4 The DDI-IO power well (§8.6 step 5)

The port's IO well is enabled through `power.rs`'s `enable_well`, which is that module's handshake
and its rollback: a well that never reports its state leaves the request bit withdrawn, so a failed
attempt does not leave the device half-powered. The error carries `power.rs`'s own
`WellStateNeverSet`. This is the one thing in the sequence that is another module's mechanism rather
than another module's registers, and calling it keeps the ordering in one place.

### 3.5 The lanes and the buffer translation (5.3)

`PORT_CL_DW5`'s `SUS_CLOCK_CONFIG` is set to `0b11` (§8.5 step 3) with a read-modify-write, because
`CL_POWER_DOWN_ENABLE` is bit 4 of the same register and §8.3 step 7 set it during phase 1.

The swing writes follow §8.5's steps 4 to 6 in the shape i915's
`icl_ddi_combo_vswing_program` gives them: `PORT_TX_DW5`'s **group** instance with TX training
disabled, then `PORT_TX_DW2`, `PORT_TX_DW4` and `PORT_TX_DW7` written once per lane in lane
order, then the group `PORT_TX_DW5` with training enabled — that last write is what commits the
settings. §8.5 step 2 demands the per-lane form for `PORT_TX_DW4` in capitals and explains it
(each lane's loadgen select differs), and i915 writes all three dwords that way, one
read-modify-write per lane over `ln = 0..3` (`[I915]` `display/intel_ddi.c:1148-1178`). The
group instances of `DW2` and `DW7` are different addresses from the lane instances
(`[I915]` `display/intel_combo_phy_regs.h:96-105`) and are not written. `DW5` is the exception
in both directions: it is read from lane 0 and written to the group
(`[I915]` `display/intel_ddi.c:1141-1146`, `:1218-1229`), which is the shape `swing.rs`'s read
and this write share.

Then `PORT_CL_DW10`'s `PWR_DOWN_LN_MASK` powers the lanes: `0x0` for four lanes, `0xC` for two,
`0xE` for one (§8.6 step 7).

**An annotation to §8.6.** §8.6 step 7 prints `PWR_UP_ALL_LANES (0x0)`, `PWR_DOWN_LN_3_2 (0xC)`,
`PWR_DOWN_LN_3_2_1 (0xE)`, and §8.2 puts the field at `[7:4]`. Those parentheses are the *field*
values; the register write is the field shifted into place, so two lanes write `0xC0`, not `0xC`.
Taking §8.6 literally would set bits 2 and 3, which are not `PWR_DOWN_LN_MASK` at all. The module
shifts, and `the_lane_power_field_is_shifted_into_place_and_leaves_other_bits_alone` pins all three
widths with an unrelated bit set in the register to prove the read-modify-write keeps it.

### 3.6 The encoder-side registers (5.4 to 5.7)

| Register | Value for HDMI, four lanes, both syncs positive | Source |
|---|---|---|
| `TRANS_CLK_SEL(A)` | `0x10000000` = `(PORT_A + 1) << 28` | §6.3 routing step 2, §11 5.4 |
| `TRANS_DDI_FUNC_CTL(A)` | `0x88030006` = `ENABLE \| SELECT_PORT(A) \| MODE_SELECT_HDMI \| BPC_8 \| PHSYNC \| PVSYNC \| PORT_WIDTH(4)` | §8.4's corrected table, §11 5.5 |
| `TRANSCONF(A)` (`PIPECONF_A`) | `0xC0000000` = `ENABLE \| STATE_ENABLE` | §11 5.6, §8.4's correction |
| `DDI_BUF_CTL(A)` | `0x80000000 \| level << 24 \| width \| A_4_LANES` | §8.4, §11 5.7 |

`0x88030006` and `0xC0000000` are `[MEASURED]` from the host test.

The polarity bits follow the mode: the module reads `mode.hsync_positive` and `mode.vsync_positive`,
which §6.1 says is where they belong (`timing.rs` deliberately does not carry them). A test plans
CTA-861 VIC 16 and VESA DMT 0x52 — the same timing published with opposite polarities — and asserts
the two bits move and nothing else does.

**Two disagreements in the reference, both followed the corrected way.**

* §8.6 step 12 still writes `TRANSCONF = ... | 8bpc`; §8.4's correction and §11 phase 5.6 both put
  the output bit depth and dithering in `PIPE_MISC` on Gen12. `TRANSCONF` is written as enable plus
  state-enable only. `PIPE_MISC` is the pipe workstream's register and is not written here.
* §8.4's `[INF]` note and §8.6 step 13 both put `PHY_LINK_RATE` in the enable write, but §8.6's
  table of encodings has eight *DisplayPort* link rates and no HDMI TMDS entry. See §5.

`IS_IDLE` is polled after the write, with the 500 µs budget §8.6 step 14 gives for HDMI. Reading
the register that was just written is §2.2's read-back discipline, and the poll is what establishes
the device saw the enable. A test drives a mock that takes three reads to come out of idle, so the
retry path is exercised rather than assumed.

## 4. The failures, and what they say

Every failure is a named error with a `describe()` in the style of `pll::PllError` and
`power::PowerError`. Nothing panics, and nothing on this path can: on this machine the log is on the
screen the mode was supposed to light up.

| Error | What it means, and what the text says to do |
|---|---|
| `UnsupportedDdi` | C or D: not a combo-PHY port (§8.1), Type-C/DKL deferred (§8.8). Refused before any write. |
| `PllConfigRegisterMissing` | Combo PHY B: the table has no `DPLL1_CFGCR0`/`CFGCR1` offset. §13.4's dump is the fix. |
| `MissingBufferTranslation` | §8.5's HDMI values are a `[GAP]`. Supply them, do not guess. |
| `HdmiScramblingNotImplemented` | The pixel clock is at or above the scrambling threshold; the sink-side SCDC enable does not exist here. |
| `Pll(PllError)` | `pll.rs` refused the mode, the reference or the encoding. |
| `PllPowerNeverCameUp` | `POWER_STATE` never set: the divider write would be dropped by an unpowered block. |
| `PllNeverLocked` | Carries `(P, Q, K)` and the reference, and names §11 phase 5.1's two usual causes. |
| `Well(PowerError)` | The DDI-IO well handshake failed, with `power.rs`'s diagnosis. |
| `DdiNeverIdle` | Names the DDI, the register, what was written and what was read, then §11 phase 5.7's advice. |
| `Unreadable` / `WriteRefused` | A register outside the window, or one that refused the write. |

`DdiNeverIdle`'s text is deliberately the longest. §11 phase 5.7 records a real `46d0` field report
of exactly this poll timing out under coreboot + EDK2 (i915 bug #10932), and its advice is that a
wrong port/`aux_ch` mapping is a far more common cause than a wrong divider — so the error tells the
reader to check the mapping and the PLL, in that order, before the arithmetic.

## 5. Every value this module could not source

This is the list the task asked for: each item is a value that is *not* in the reference document,
what was done instead, and what would close it.

1. **The HDMI buffer-translation values.** §8.5 selects `icl_combo_phy_trans_hdmi` and then says in
   as many words *"`[GAP]` I did not extract its values"*; §13.1 item 12 repeats it. They are the
   voltage-swing and pre-emphasis numbers, and §8.5's own note records that the DG1 PRM and i915's
   ADL-P table disagree about them for the same nominal level, so they are board-tuned rather than
   derivable. **What was done:** `SwingProgram` is a caller field, and a request without one is
   refused with `MissingBufferTranslation` before any write. **What closes it:** a register dump of
   a working configuration (§13.4) — the same route §6.3 route 1 recommends for the dividers. The
   tests use invented values whose `source` string says so, and they exercise the write order only.
2. **The `DW5` `TX Training Enable` and `Scaling Mode Sel` bit positions.** §8.5 names both fields
   and gives neither position, so the two `PORT_TX_DW5` states are caller-supplied dwords
   (`dw5_training_disabled`, `dw5_training_enabled`) rather than a value plus a bit computed from an
   unstated position.
3. **The bit positions inside `PORT_TX_DW2`/`DW4`/`DW7` for the fields §8.5's table carries**
   (`dw2_swing_sel`, `dw7_n_scalar`, `dw4_cursor_coeff`, the two post-cursor values). §8.5 gives the
   *values* for DisplayPort and no packing, which is a second reason the swing program takes dwords.
4. **Which register set actually carries the translation on a combo PHY.** §8.4 declares the
   indexed `DDI_BUF_TRANS_LO/HI` pair and calls them "de-emphasis / balance leg" and
   "vref / vswing"; §8.5's write sequence programs the PHY's `PORT_TX_DW2`/`DW4`/`DW5`/`DW7`
   instead. `regs/ddi.rs`'s transcription notes already record that the document does not decide
   which one a port uses. This module programs the PHY TX registers, because that is the sequence
   §8.5 gives step by step, and still puts the level index in `DDI_BUF_CTL.BUF_TRANS_SELECT` as
   §8.4 requires. **What closes it:** a dump, again — read both register sets from a working mode
   and see which holds the values.
5. **`DDI_BUF_CTL.PHY_LINK_RATE` for HDMI.** §8.6 step 13 writes the field; §8.6's encoding list is
   `162000→0, 216000→4, 243000→5, 270000→1, 324000→6, 432000→7, 540000→2, 810000→3`, all
   DisplayPort link rates, and a 148.5 MHz HDMI mode is not in it. **What was done:** `[INF]` the
   field is written as **0**, its reset value, and the log says so; `LinkRate::Code` lets a caller
   supply a code from a dump instead. It is the only value the sequence chooses for itself that the
   document does not give, and the log labels it as an inference rather than hiding it. It is not a refusal because the field tunes
   the DDI buffer's own equalisation, and refusing a whole bring-up over an equalisation margin
   would be the wrong trade.
6. **`DPLL1_CFGCR0`/`DPLL1_CFGCR1`.** §6.3 states the config offsets for DPLL0 only. `pll.rs`
   computes PHY B's dividers happily; there is no inventoried address to write them to.
   `regs/dpll.rs` already records the omission, and this module refuses PHY B with
   `PllConfigRegisterMissing` rather than guessing a neighbouring offset. **This is the one gap that
   blocks a whole port**, and it matters if the monitor turns out to be on DDI B.
7. **The `DDI_CLK_SEL` field's per-PHY siblings.** §6.3 gives one `ICL_DPCLKA_CFGCR0` at `0x164280`
   with both PHYs' fields inside it. No `DPCLKB`/`C`/`D` register appears anywhere in the document,
   so none is declared or written.
8. **The ADL-N-specific values the reference already lists as `[GAP]`** and which this module
   inherits rather than closes: the reference frequency (§13.1 item 4 — read from `SKL_DSSM`, which
   the module does), which ports the SKU wires up (§13.1 item 3 — decided by the sink workstream's
   hotplug and EDID reads), and the power-well map (§13.1 item 1 — `power.rs`'s).

## 6. What the tests measure

`[MEASURED]` 283 `drm::intel` host tests pass, 31 of them this module's, none failing. `cargo
clippy` for the host test target and for the product kernel configuration (`tools/thekernel.py
lint`, `x86_64-unknown-none`, release) reports nothing in `output.rs` or `output/tests.rs`. The
product kernel builds.

The 31 tests assert, among other things:

* the write order, on `writes()` rather than on the return value: twenty-nine writes in the order
  §8.6 gives, with the DDI-IO well between the clock mapping and the swing values;
* the PLL is powered before it is enabled and the dividers land in between;
* `ICL_DPCLKA_CFGCR0` is written exactly twice, with the gate bit set in the first write and cleared
  in the second;
* the per-lane `PORT_TX_DW2`, `PORT_TX_DW4` and `PORT_TX_DW7` writes carry the four supplied values
  each, in lane order, and the two group `PORT_TX_DW5` writes straddle them;
* one lane's write is not another lane's: after the sequence each lane address holds its own value,
  the lane addresses are at the `0x100·ln` stride, and the group instances of `DW2` and `DW7` are
  never written;
* the two group `PORT_TX_DW5` writes carry the word the read route took from lane 0, not the group
  instance's own word, with `swing::read_firmware_swing` → `plan` → `program` run end to end on one
  mock whose two `DW5` instances differ;
* the lane-power field is shifted into `[7:4]` and an unrelated bit in `PORT_CL_DW10` survives;
* `SUS_CLOCK_CONFIG` does not clobber `CL_POWER_DOWN_ENABLE`;
* the polarity bits follow the mode in both directions and change nothing else;
* a DDI that is not a combo-PHY port is refused before any write — including a tampered plan, which
  is how the re-check inside `program` is shown to be real;
* `IS_IDLE` never clearing is `DdiNeverIdle`, whose text names the DDI, the register, the value read
  and §11 phase 5.7's advice; and a DDI that takes three polls succeeds;
* `PLL_LOCK` never setting is `PllNeverLocked` carrying `(P, Q, K)` and the reference; and
  `PLL_POWER_STATE` never setting is a *different* error, because the dividers were never reached;
* the DDI-IO well that never comes up leaves its request bit withdrawn;
* the plan log carries `ref`, `(P, Q, K)`, the symbol rate and both `CFGCR` values.

### 6.1 One measurement that is a finding: `pll.rs` is not the ADL-N search

`[REF]` §6.3's corrections 1 and 2 record that ADL-N uses `icl_calc_wrpll`: a flat divider list
ending at 21, the DCO window `[7998, 10000] MHz`, and selection by distance from the 8999 MHz
midpoint. `pll.rs` implements the **Skylake** search instead — three central frequencies
(8400/9000/9600 MHz), an asymmetric `+1%/−6%` tolerance, and a divider list that includes 35, which
§6.3 says the ADL-N list does not contain. `pll.rs`'s own documentation says so; what was missing was
a number.

`output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search` supplies it, by
running both searches over the same inputs and counting. `[MEASURED]`, over 985 symbol rates from 16
to 1000 MHz in 1 MHz steps:

| Count | Value |
|---|---|
| rates both searches can make | 574 |
| of those, a different total divider | 114 |
| of those, `pll.rs`'s choice also outside the PRM's `[7998, 10000] MHz` window | 12 |
| rates where `pll.rs`'s choice is outside that window (all 12 are disagreements) | 12 |
| rates only `pll.rs`'s list reaches | 7 |
| rates only the documented ADL-N list reaches | 245 |
| rates neither can make | 159 |

And over the modes this kernel actually publishes (the DMT and CTA-861 tables, interlaced rows
skipped because `timing.rs` refuses them anyway):

| Count | Value |
|---|---|
| progressive modes both searches can make | 181 |
| of those, a different total divider | 56 |
| modes only `pll.rs` can make | 0 |
| modes only the documented search can make | 1 — CTA VIC 92, 2560×1440@120, 495 MHz, which this sequence refuses at the scrambling gate anyway |

**What this means for this bring-up:** the target mode is one where the two agree — both land on
total divider 12, and the register values match §6.3's worked example exactly — so phase 5 does not
depend on the divergence. What it means for the mode layer is that `pll.rs` cannot make some modes
the documented ADL-N arithmetic can, and picks a different divider (occasionally outside the PRM's
own DCO window) for a fifth of the rates both can make — and for 56 of the 181 published
progressive modes, nearly a third. That is WS-1's module and a decision for the coordinator;
it is measured here because this module is the first caller, and a silent difference in the clock
would surface as a monitor reporting "out of range" with no other symptom.

The test pins all of these numbers, so if `pll.rs` is brought onto the ADL-N path the test fails and
the failure is the finding.

## 7. What is not verified

* **Nothing has run on hardware.** Not a register, not a poll, not a divider. Every claim about the
  sequence is a claim about the reference and about a mock.
* **The mock's status bits are the ones the tests install.** `POWER_STATE` follows `POWER_ENABLE`,
  `LOCK` follows `PLL_ENABLE`, the DDI leaves idle when it is enabled, the IO well reports state when
  requested. Whether the real bits behave that way, and how long they take, is unmeasured. The poll
  budgets are the reference's figures, not observations.
* **The two caller-supplied value sets** — the swing program and any `PHY_LINK_RATE` code — are
  untested by construction; the tests use invented numbers to exercise the write order and say so in
  the logged `source`.
* **The byte-level effect of `TRANS_DDI_FUNC_CTL`, `TRANSCONF` and `DDI_BUF_CTL`** is unverified:
  the values match the field tables, which is a statement about the document, not about the wire.
* **The `[INF]` choices** (writing `PHY_LINK_RATE` as 0; the scrambling threshold at 340 MHz rather
  than §8.4's `[INF]` "approximately 300 MHz") are labelled inferences with the dump that would
  settle them named in §8.
* **The order of the swing writes within §8.5 step 5** follows i915's
  `icl_ddi_combo_vswing_program` — group `DW5`, then `DW2`, `DW4` and `DW7` per lane
  (`[I915]` `display/intel_ddi.c:1141-1178`) — rather than §8.5's own list, which gives the
  batch and its commit write and no required interleaving of the three dwords.
* **`DW5`'s named fields are replayed, not set.** i915 clears and sets `SCALING_MODE_SEL`,
  `RTERM_SELECT`, `TAP2_DISABLE` and `TAP3_DISABLE` on the group instance after reading lane 0
  (`[I915]` `display/intel_ddi.c:1141-1146`). This module writes the two `DW5` states it is
  given and derives nothing from an unstated bit position, so on the read route those fields
  hold what the firmware's lane-0 word held, with only `TX_TRAINING_EN` toggled. A firmware
  whose values differ from i915's is replayed rather than corrected — that is the read route's
  premise, argued in `docs/design/intel-swing.md` §4.
* **The end-to-end product build was not completed.** `tools/thekernel.py lint` compiles and lints
  the kernel for `x86_64-unknown-none` in release with the product features, and that succeeds with
  nothing reported in these files; `tools/thekernel.py build` stops earlier than the kernel, in
  `scripts/build-rootfs.sh`, on a TLS error while downloading the root filesystem image. That
  failure is unrelated to this change and no QEMU run was attempted.

## 8. What to read on the machine, first

`[REF]` §13.4's advice, narrowed to what would settle this module. None of it has been done.

1. **A full dump of a working configuration**, `0x00000`–`0x7FFFF` and `0xC0000`–`0xCFFFF`, taken
   while the firmware's own output is up. It settles §5 items 1, 3, 4 and 5 at once: the
   buffer-translation values from whichever register set holds them, the packing of the TX fields,
   and the `PHY_LINK_RATE` code.
2. **`DPLL0_CFGCR0`/`DPLL0_CFGCR1` from that dump.** If the firmware programmed 1080p60, the two
   words should be `0x001001D0` and `0x00000E84`, and the `KDIV`/`PDIV` fields inside the second one
   settle §3.2's encoding question the way §6.3 predicts. If the second is `0x00000E44`, then
   whatever programmed it used the Skylake codes, and `pll.rs`'s `Executed` encoding is the one this
   machine's firmware agrees with.
3. **`DDI_BUF_CTL(A)` and `DDI_BUF_CTL(B)`** from that dump, for `IS_IDLE` and `PHY_LINK_RATE`.
4. **`ICL_DPCLKA_CFGCR0`** from that dump, to confirm the `DDI_CLK_SEL` shift and the `DDI_CLK_OFF`
   bit positions per PHY, and to see which DDI the firmware used.
5. **If the monitor is on DDI B:** read `0x164290`–`0x1642A0` in the dump and find the pair whose
   fields look like `DPLL0_CFGCR0`/`CFGCR1` do. That is the one gap that blocks a port outright, and
   a single dump closes it. The offsets must then go into `regs/dpll.rs` and the table, not into
   this module as a literal.
6. **`DPLL0_ENABLE` after the firmware's modeset**, to confirm the lock bit and to compare the poll
   timing the reference reports against what this machine does.

### 8.1 A route that needs no offline dump

The firmware on the target machine drives the same HDMI output before this kernel does, so the
translation values that matter for *this* board are already in the PHY when the kernel starts. They
can be read at boot — the two candidate sets are the PHY's per-lane `PORT_TX_DW2`/`DW4`/`DW7`
(`0x162888 + 0x100*ln`, `0x162890 + 0x100*ln`, `0x16289c + 0x100*ln`) together with `PORT_TX_DW5`
lane 0 (`0x162894`), all of them minus `0xf6000` for PHY B, and the indexed `DDI_BUF_TRANS_LO`/`HI`
pair (`0x64E00 + i*8` and `+4`, `0x64E60` for port B; the table declares entries 0 to 9) — and fed
into `SwingProgram` with a `source` string that says where they came from. Reading both sets and
logging the raw words answers §5 item 4 at the same time: whichever set holds plausible per-lane
variation is the one the port uses.

That is a route for whoever wires this into the boot sequence, not something this module does. It
reads no register the sequence does not own and writes nothing it did not compute; what it must not
do is invent the numbers, which is why a request without them is refused.
