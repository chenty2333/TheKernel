# Intel display power, clocks and combo PHY initialisation

**Status: none of this has run on the target machine.** The target is an Acer
蜂鸟mini (SQM2270) with an i3-N305 (`8086:46d0`, Gen12 / Xe-LP, display IP
version 13) that the coordinator will boot from USB.  Every claim below is a
claim about a host test, a reading of `docs/design/intel-display-registers.md`,
or a reading of the source that document cites.  Nothing here has been
validated against silicon, and no register value in this workstream has been
read from or written to a real Intel display engine.

This document covers one workstream: display power wells, CDCLK and raw clock,
and combo PHY initialisation — reference §4 (Power), §8.3 (Combo PHY
initialisation) and §11 phases 0.3 and 1.  It is the workstream everything else
in stage 2 sits behind, because a register read while a power well is down
returns zero (§4.2), which is indistinguishable from a device that is not
there.

## 1. What was implemented, and where

| file | what it owns |
|---|---|
| `kernel/src/drm/intel/power.rs` | `bring_up`, the well handshake, DC states, DBUF slices, the platform workarounds, and the bring-up log |
| `kernel/src/drm/intel/clk.rs` | `SKL_DSSM` reference decode, the ADL-N CDCLK ratio table, the CD2X divider and decimal arithmetic, the CDCLK PLL, `PCH_RAWCLK_FREQ` |
| `kernel/src/drm/intel/phy.rs` | combo PHY initialisation, the process/voltage reference table, the verification pass |
| `kernel/src/drm/intel/regs.rs` | the registers themselves, the `Registers` trait, the bounded poll, and the host mock |

`power::bring_up` is the single entry point.  It is **not wired into the boot
path**: nothing in this kernel calls it yet, so nothing in this kernel writes a
display register at boot.  That keeps the probe's stage-1 claim ("no register
was written") true and leaves the decision to call it to the coordinator, as
the workstream brief requires ("Do not modify files outside your slice"; the
call site would be `probe_at_boot` in `mod.rs`, which is more than a `mod`
line).

## 2. The sequence, and why the order is the deliverable

Reference §4.9's `icl_display_core_init` and §11's phases 0.3 and 1.  Each step
names the function that performs it and the register it touches.

| # | step | code | registers |
|---|---|---|---|
| 0.3 | fuse and strap readback | `power::read_fuses` | `SKL_DFSM` `0x51000`, `SFUSE_STRAP` `0xC2014`, `SKL_DSSM` `0x51004`, `SKL_FUSE_STATUS` `0x42000` |
| 1.1 | DC states off | `power::disable_dc_states` | `DC_STATE_EN` `0x45504` |
| 1.2 | combo PHY init, PHY A first | `phy::init_all` | PHY A `0x162000`, PHY B `0x06C000`, `ICL_PHY_MISC` `0x64C00`/`0x64C04` |
| 1.3 | `PW_1` | `power::enable_well(PW_1)` | `GEN8_CHICKEN_DCPR_1` `0x46430`, `HSW_PWR_WELL_CTL2` `0x45404`, `SKL_FUSE_STATUS` |
| 1.2b | PHY `COMP_INIT` re-read | `power::bring_up` | `PORT_COMP_DW0(A/B)` |
| 1.4 | CDCLK | `clk::bring_up` | `SKL_DSSM`, `CDCLK_PLL_ENABLE` `0x46070`, `CDCLK_CTL` `0x46000` |
| 1.4b | raw clock | `clk::bring_up_raw_clock` | `PCH_RAWCLK_FREQ` `0xC6204` |
| 1.5 | DBUF slices | `power::enable_dbuf` | `DBUF_CTL_S0..S3` `0x45008`, `0x44FE8`, `0x44300`, `0x44304` |
| 1.6 | platform workarounds | `power::apply_workarounds` | `GEN11_CHICKEN_DCPR_2` `0x46434`, `XELPD_DISPLAY_ERR_FATAL_MASK` `0x4421C` |

The order is not a style choice.  §4.2 states it plainly: **when a power well is
down, writes to its registers are dropped and reads return zero.**  That is why
a wrong power-well sequence presents as "every register reads 0" — the failure
mode §4.2 says is most often misdiagnosed as a dead device.  So a step that
reads a register before the well that gates it is a step that reads nothing,
and the sequence is the difference between a value that means something and one
that does not.

Two orderings inside the sequence are worth spelling out:

* **The PHYs are initialised before `PW_1` is enabled.**  That looks backwards
  and is what `icl_display_core_init` does (step 2 before step 3).  The
  consequence is that a `COMP_INIT` that does not stick at that point means
  nothing — §11 phase 1.2 says the cause is a missing `PW_1`, which has not
  been asked for yet.  `bring_up` therefore re-reads `COMP_INIT` after `PW_1`
  and reports both readings, which turns a known-ambiguous check into a
  meaningful one.
* **`Wa_14011508470` is last.**  `GEN11_CHICKEN_DCPR_2` is programmed after
  CDCLK and DBUF are up, which is where `icl_display_core_init` applies it
  (step 10).  A test asserts the ordering from the register writes themselves.

### 2.1 The XE_LPD power well map, and why it is not Tiger Lake's

`PW_1` is index 0 with request bit `0x2` and state bit `0x1`; `PW_2` is index 1;
`PW_A`..`PW_D` are indices 5..8.  Indices 2, 3 and 4 belong to no `XE_LPD` well
at all.

This is the single most dangerous place in the slice to generalise, and §4.2.1
says so: Tiger Lake, Rocket Lake and ADL-P/N are all Gen12 and all three map
the same bits differently.  Tiger Lake's chain is `PG1` = 0, `PG2` = 1,
`PG3` = 2 and so on; Rocket Lake has no `PG2` and no `PG5`.  A driver that
carried Tiger Lake's numbering to ADL-N would enable the wrong well, read zeros
everywhere, and have nothing in the log to say why.  The table in `power.rs` is
`XE_LPD`'s (`[I915]` `i915_reg.h:3650-3660`,
`display/intel_display_power_map.c:1325-1410`), and a test pins it against
Tiger Lake's shape.

## 3. Decisions a reviewer should check

### 3.1 A poll budget is a count of reads, not a clock reading

Every poll in this workstream is bounded by a count derived from the documented
microsecond timeout (`regs::poll_attempts`, `POLL_COST_US = 1`).  Two
consequences are deliberate:

* The sequence cannot hang because a clock is unavailable, and it behaves
  identically on the target and in a host test.  This kernel's platform time
  interface is registered only for `target_os = "none"`
  (`crates/ax/thekernel-axplat-x86-pc/src/time.rs`, `impl_plat_interface` is
  behind `cfg_attr(target_os = "none", ...)`), so a poll that read
  `monotonic_time` would not be host-testable at all — and the handshake's
  success path, its timeout path and its rollback are precisely what has to be
  testable without hardware.
* The bound stays traceable: each caller passes the microsecond figure the
  reference gives for that status bit, and the count is that figure divided by
  the assumed cost of one uncached MMIO read.

What it is not is a precise timer.  A failed poll means "the status did not
appear within N reads", and the error says so.  Where two sources give two
timeouts, the longer one is used and the shorter one is named in the constant's
comment: `[PRM]` gives 30/45 µs for the `PW_1` state bit and 20 µs for the fuse
bits, while `[I915]` uses 1 ms for both.  A late well is not a failure, and a
bound that is too tight would refuse hardware that works.

### 3.2 Which poll failures stop the sequence

| poll | on failure | why |
|---|---|---|
| `PW_1` `STATE` | **fatal** | the hardware's own statement that the well is up; continuing past it reads zeros for the rest of bring-up |
| `PG0` fuse (before the request) | **fatal** | the root of the tree; nothing under it is powered |
| a well's own `PG` fuse | recorded | `[I915]` `gen9_wait_for_power_well_fuses` warns and continues; for the wells above `PW_1` the bit position is an `[INF]` |
| DBUF slice `POWER_STATE` | recorded | fatal only if *no* slice comes up |
| PHY `COMP_INIT` | recorded | re-read after `PW_1`, where a zero becomes meaningful |
| CDCLK PLL `LOCK` | **fatal** | an unlocked PLL means the display has no clock |
| `DC_STATE_EN` read-back | **fatal** | with DC states enabled the hardware may power-gate what is being programmed, intermittently |

Nothing is silently ignored: a recorded failure is carried in `PowerState` and
printed, naming the bit and the value that was read.

### 3.3 Rollback: a failure puts the well back

A bring-up that fails after `PW_1` came up withdraws the request bit it added
(`power::unwind`), so the display is left as it was found rather than powered
and unprogrammed.  The same rollback happens inside the handshake: a well whose
`STATE` bit never sets has its request withdrawn before the error is returned,
because leaving the bit set would make the next attempt — and anyone reading
the register afterwards — unable to tell whether it was there before, which is
the difference between a retry and a diagnosis.

Two details make it safe rather than destructive:

* The request is withdrawn only when *this call* added it.  A bit that was
  already set belongs to whoever set it — the firmware, or an earlier attempt —
  and is left alone; the error says which of the two happened.
* Nothing else is unwound.  The DBUF slice requests are left, because with
  `PW_1` down their state reads zero and `enable_dbuf` reads the state before it
  requests anything, so a retry re-requests exactly what it needs.  A
  half-programmed PHY is left for the same reason, and because clearing
  `COMP_INIT` on a PHY whose reference values are half-written would make a
  retry's verification pass fail for a reason this driver created.

The diagnostic is read *before* the rollback in both cases: a report that said
"nobody requested this well" because the rollback had already run would be
actively misleading about the one thing §11 phase 1.3 asks a reader to compare.

### 3.4 Read-modify-write convention

`rmw(regs, register, clear, set)` computes `(read & !clear) | set`, which is
`[I915]`'s `intel_de_rmw(reg, clear, set)` rather than a "field mask and value"
helper.  The distinction is load-bearing: `icl_combo_phys_init` writes
`rmw(COMP_DW8, 0, IREFGEN)`, meaning "set this bit and clear nothing", which a
mask-and-value helper silently turns into a no-op.  The convention is documented
at both call sites.

### 3.5 The firmware's CDCLK is kept

`bxt_cdclk_init_hw` sanitises and then returns early if the PLL is enabled and
locked with a VCO the table knows (§4.6, §11 phase 1.4).  `clk::bring_up` does
the same three checks and keeps the firmware's CDCLK when they hold, writing
nothing.  A wrong `CDCLK_FREQ_DECIMAL` field is reported and *not* corrected:
the frequency is what the display runs on, and rewriting the field on a running
display is a CDCLK change, which the PRM says requires disabling every display
engine function first.

### 3.6 The raw clock: strap over firmware, and the disagreement is logged

§4.8 and §13.3 record a source disagreement: the DG1 PRM expects a 38.4 MHz raw
clock, while `[I915]`'s `cnp_rawclk` derives 24 or 19.2 MHz from
`SFUSE_STRAP[8]`.  The strap is used, because the code path exists precisely
because the value varies between platforms.  `PCH_RAWCLK_FREQ` is read first
and reported; it is written only when it disagrees with the strap, and a
disagreement is printed as such, so a machine where the two differ is visible
rather than silently overruled.

A consequence worth knowing: `intel_pch_rawclk`'s DG1 path programs
`DEN(4) | DIV(37) | NUM(2)` for 38.4 MHz, and that value does *not* decode to
38.4 MHz under the arithmetic that builds this driver's two encodings.  So a
register value that is not one of the two this driver can produce is carried
raw and reported as undecodable rather than guessed at.

### 3.7 `XELPD_DISPLAY_ERR_FATAL_MASK` is deliberately left unmasked

`[I915]` writes all-ones there (`Wa_14011503030`), masking every fatal display
error.  §4.9 step 11 suggests the opposite for a first bring-up and §13.2 marks
that suggestion as `[INF]`.  This driver takes the `[INF]`: an error that is
masked is an error nobody sees.  The register is *read*, so the log says what
state it was left in rather than implying the question was never asked.

### 3.8 The PCode handshake is not implemented

`[I915]` `bxt_set_cdclk` opens with
`skl_pcode_request(SKL_PCODE_CDCLK_CONTROL, SKL_CDCLK_PREPARE_FOR_CHANGE, ...)`
and closes by writing the voltage level.  This kernel has no PCode mailbox, so
neither happens.  The programming path used instead is the short one §11 phase
1.4 gives for a machine with no pipe running, and it is only reached when the
firmware left no usable CDCLK — which on a machine whose firmware drove the
screen should not happen.  Related: CDCLK *crawl* (`has_cdclk_crawl` is set for
`XE_LPD`) is unreachable by construction, because `bxt_de_pll_readout` reports a
VCO of zero unless the PLL is enabled *and* locked, so every state that reaches
the programming path is one i915 also treats as a disable/enable.

### 3.9 Two steps of `icl_display_core_init` are not ported

`icl_mbus_init` (step 6) and `tgl_bw_buddy_init` (step 7, `BW_BUDDY_CTL`
`0x45130`/`0x45140`) are in `icl_display_core_init` but not in §11's phase 1,
and neither belongs to this slice's registers.  If a running pipe underruns or
the memory arbiter misbehaves, they are the first unported steps to look at.

Similarly, `hsw_power_well_post_enable`'s VGA reset and its per-pipe interrupt
power-well handling (`gen8_irq_power_well_post_enable`) are not done: they
belong to the VGA and interrupt workstreams.

## 4. `[GAP]` and `[INF]` that had to be bridged

Each is marked in the code where it is used.

1. **The ADL-N power-well map must be verified on hardware, not inferred**
   (§13.1 item 1, §4.2.1).  Bridged by using i915's `XE_LPD` map, by pinning it
   against Tiger Lake's in a test so the choice is visible, and by making the
   `STATE` bit — not the map — the thing that decides success.
2. **The `PG6`..`PG9` fuse bit positions for `PW_A`..`PW_D`** (§4.4, marked
   `[INF]`).  Bridged by computing them from the documented
   `SKL_FUSE_PG_DIST_STATUS(pg) = 1 << (27 - pg)` with `pg = idx + 1`, and by
   recording rather than failing on those polls; a failure names the bit that
   was clear so a hardware session can check it directly.
3. **The ADL-N CDCLK reference frequency** (§4.6, §13.1 item 4).  Bridged by
   reading `SKL_DSSM[31:29]` and never assuming.  The three undefined encodings
   fall back to 24 MHz as `icl_readout_refclk` does, but the fallback is flagged
   and printed, and the flag is asserted in a test.
4. **The ADL-N raw clock frequency** (§4.8, §13.1 item 5).  Bridged as §3.6
   above.
5. **The ADL-N CDCLK voltage-level table** (§4.6, §13.1 item 6).  Bridged by
   not computing one: no PCode write happens, so no voltage level is needed.
   If a PCode mailbox is added, this is the missing piece.
6. **Which DBUF slices a given SKU populates** (§4.7, §13.1 item 3).  Bridged by
   reading each slice's state before requesting it, enabling all four from
   `XE_LPD`'s slice mask, and treating a partial result as a report rather than
   a fault while at least one slice is up.
7. **`DBUF_TRACKER_STATE_SERVICE` for ADL-P/N** (§4.7, §13.1 item 8).  Not
   programmed at all: `gen12_dbuf_slices_config` returns early for ADL-P, the
   reference marks where ADL-P programs it as unknown, and §4.7's advice is to
   leave it at its reset value and change it only if underruns are observed.
8. **`CD2X_DIV_SEL` encodings `01b` and `11b`** (§4.6, marked `[INF]` because
   the TGL/DG1 PRMs list only two of the four).  Bridged by treating all four as
   legal — i915 names all four and the two ADL-N needs for its lowest entries
   are `10b` and `01b`, one of which is in the `[INF]` pair — and by printing the
   decoded divider so a disagreement would be visible.
9. **Combo PHY presence** (§12.2, marked `[INF]`).  See §5 below.
10. **The combo PHY DCC step** (§13.3: the PRM volumes contradict each other,
    "DCC continuous mode" versus "divide by 2").  Bridged by following i915,
    which §13.3 calls unambiguous and Gen12-targeted: `RUN_DCC_ONCE` in
    `PORT_PCS_DW1`.

## 5. Where this deviates from the reference document, and why

### 5.1 A defect in the reference: the `PORT_TX` sub-block base

§8.2's table gives the `PORT_TX_DW*` group base as `+0x400`.  The header that
table cites for itself (`[I915]` `display/intel_combo_phy_regs.h:96-105`)
defines `_ICL_PORT_TX_GRP` as `0x680` and `_ICL_PORT_TX_LN(ln)` as
`0x880 + ln * 0x100`.  `0x400` would address the tail of the `PORT_PCS` region
and write the wrong register entirely.

This driver uses the header's offsets.  A test
(`a_combo_phys_registers_are_at_the_documented_sub_block_offsets`) recomputes
every PHY register offset from the base-plus-sub-block rule, so a transcription
slip in either direction fails.  `PORT_TX_DW8` for PHY A is therefore `0x1626A0`,
not `0x162420`.

### 5.2 A contradiction in the reference: forcewake for the PHY and DPLL blocks

§2.1 quotes `NEEDS_FORCE_WAKE(reg) = (reg < 0x40000 || reg >= 0x116000)` and
concludes that the combo PHY registers (PHY A at `0x162000`) and the DPLL
configuration registers (`0x164xxx`) need forcewake.  §3.3 quotes i915's
`__gen12_fw_ranges` as `GEN_FW_RANGE(0x40000, 0x1bffff, 0)` — domain zero, no
forcewake — which includes both blocks.  The two cannot both hold.

This driver follows the band the probe already encodes
(`regs::FORCEWAKE_FREE_BANDS`, `0x40000`–`0x1BFFFF`) and i915's range table.
The register table's compile-time assertions keep every register inside that
band.  **If a combo PHY or DPLL write ever reads back as zero on hardware, this
contradiction is the first thing to revisit** — it is exactly the "reads zero
for the wrong reason" failure §4.2 warns about, and the two candidate
explanations (a gated block, or a dropped write) look identical from the log
alone.

### 5.3 Combo PHY presence is not probed with a scratch write

§12.2 suggests, marked `[INF]`, writing a known pattern to an unused
`PORT_COMP_DW1` bit and reading it back to tell an absent PHY instance from one
that is merely in reset.  This driver does not do that:

* On ADL-N the two combo PHYs are known from sources — `[I915]`'s
  `xe_lpd_display` port mask, and `intel_combo_phy_regs.h`'s own naming of the
  `C`, `D` and `E` instances as EHL's, RKL's and ADL-S's — so there is nothing
  to discover.
* A scratch write would have to be undone before `COMP_DW1`'s process and
  voltage fields are programmed, which is a worse risk than the one it removes.

The all-zero/all-ones read §12.2 describes *is* taken, twice, and reported — as
a warning rather than a decision.  A block that is merely power-gated reads zero
too, so skipping initialisation on that evidence would convert a diagnostic into
a failure; writes to a genuinely absent block are dropped by the bus and cost
nothing.

### 5.4 A PHY that verifies is not rewritten

`[I915]` `icl_combo_phy_verify_state` checks seven values before deciding a PHY
is already initialised, and this driver implements the same seven, reporting
which one disagreed when the answer is "not initialised".  A test breaks each of
the eight checks in turn and asserts both that programming resumes and that the
failed check is named.

## 6. What is not verified, and how to verify it on the machine

Nothing in this workstream has run on the target.  The evidence is:

* **80 host tests** across `drm::intel`, of which 45 are new in this workstream
  (16 in `power.rs`, 15 in `clk.rs`, 10 in `phy.rs` and 4 in `regs.rs`), run
  with `cargo test --locked -p thekernel-kernel --target
  x86_64-unknown-linux-gnu -- drm::intel`.  They drive the sequences through
  `regs::mock::MockRegisters`, which models a status bit that follows a request
  bit, one that never appears, a register that is not in the window, and a write
  the device drops.
* **Readings of the cited sources**, cross-checked where two sources state the
  same thing.  The procmon table is the one place in this slice where the PRM
  and i915 state values identically, and it is asserted row for row.

The tests prove that the sequences do what the sources say.  They cannot prove
the sources are right about ADL-N, and they cannot prove that a sequence which
is textually correct is correct on this part.

When the machine boots, the reads that settle the open questions are, in order:

1. `SKL_DFSM` — which pipes exist.  If every pipe is fused off, bring-up
   refuses with that as the reason and no register is written.
2. `SFUSE_STRAP` — bit 7 (headless) and bit 8 (the raw clock strap).
3. `SKL_DSSM[31:29]` — the CDCLK reference.  §4.6's warning is that assuming
   38.4 MHz when the hardware says 19.2 makes every ratio wrong by two, and
   this is the read that prevents it.
4. `SKL_FUSE_STATUS` before and after the `PW_1` request — whether `PG0` and
   `PG1` distribute.  If `PW_1`'s `STATE` bit never sets, the error enumerates
   the causes §11 phase 1.3 lists and prints all four request registers.
5. `CDCLK_PLL_ENABLE` and `CDCLK_CTL` — whether the firmware left a usable
   CDCLK, which is the expected case and which this driver then leaves alone.
6. `PCH_RAWCLK_FREQ` — what the firmware programmed, against what the strap
   says.  §12.3 calls the firmware's value the ground truth after a vendor
   driver has run; a disagreement here is logged and is the first thing to
   revisit if GMBUS or hotplug timing misbehaves later.
7. `PORT_COMP_DW0` for PHY A and PHY B before and after `PW_1` —
   §11 phase 1.2's "does `COMP_INIT` stick" check, which only means something
   after the well is up.

`bring_up` returns a `PowerState` that already contains all of these, and
`PowerState::render` produces the log; a caller that wires it into the boot path
gets the whole diagnostic as text on the console, which is the only output
device the target has.

## 7. Test inventory

| test | what it pins |
|---|---|
| `clk::the_dssm_encoding_is_the_one_the_prm_and_i915_agree_on` | the three `DSSM[31:29]` encodings, the fallback for the other five, and the bypass clock |
| `clk::the_cdclk_table_is_the_adlp_table_the_reference_reproduces` | all 15 rows against §4.6, grouped and ascending per reference |
| `clk::every_table_row_has_an_exact_legal_cd2x_divider` | `vco / cdclk ∈ {2,3,4,8}` for every row, and §4.6's worked example |
| `clk::the_decimal_field_matches_the_prm_and_i915_at_the_frequencies_both_state` | 307.2 → 612 and 172.8 → 344, plus every row against the definition |
| `clk::the_ctl_value_is_the_divider_the_pipe_and_the_decimal` | `CD2X_PIPE`'s four encodings against the PRM's bit patterns |
| `clk::a_working_cdclk_left_by_firmware_is_observed_and_kept` | a legal CDCLK survives untouched, including a wrong decimal field |
| `clk::a_cdclk_that_is_not_usable_is_recognised_as_such` | all five ways it can be unusable |
| `clk::a_usable_cdclk_is_kept_and_an_unusable_one_is_replaced` | the keep/program decision, and the two distinct read-back failures |
| `clk::programming_the_cdclk_writes_the_ratio_then_enables_then_locks` | the write sequence and its order |
| `clk::a_pll_that_never_locks_is_an_error_that_says_what_to_check` | the §4.6 failure, and that the sequence stops there |
| `clk::the_raw_clock_is_whichever_the_strap_selects` | both encodings, their register values, and that nothing else decodes |
| `clk::the_raw_clock_is_programmed_from_the_strap_and_not_overwritten_otherwise` | keep-if-correct, and the disagreement path |
| `clk::a_register_outside_the_window_is_an_error_rather_than_a_zero` | unreadable and refused-write paths |
| `clk::an_undefined_reference_encoding_is_reported_rather_than_hidden` | the fallback flag, and that it is not a licence |
| `phy::the_reference_table_is_the_one_both_sources_agree_on` | the five procmon rows |
| `phy::every_documented_process_and_voltage_selects_its_row` | all five `COMP_DW3` combinations, and that other bits do not matter |
| `phy::an_unrecognised_process_or_voltage_falls_back_and_says_so` | the fallthrough, flagged |
| `phy::initialising_a_phy_writes_the_sequence_the_reference_gives` | the nine writes, in order, with every mask-preserving detail |
| `phy::a_phy_b_is_not_a_compensation_source_and_gets_no_irefgen` | the source/sink rule in both directions |
| `phy::a_phy_that_firmware_already_initialised_is_left_alone` | the verification pass, and all seven ways to fail it |
| `phy::a_phy_whose_comp_init_does_not_stick_says_what_that_means` | §11 phase 1.2's ambiguity, named |
| `phy::a_phy_that_reads_all_zero_or_all_ones_is_reported_but_still_initialised` | the §12.2 read as a warning |
| `phy::both_phys_are_initialised_source_first` | the ordering rule |
| `phy::a_phy_register_outside_the_window_is_an_error_rather_than_a_zero` | the error paths |
| `power::a_whole_bring_up_runs_in_the_documented_order` | every phase, and the order asserted from the writes |
| `power::the_log_says_what_happened_at_every_step` | the log's content and its prefix |
| `power::a_machine_whose_display_is_fused_off_is_refused_before_anything_is_written` | the phase 0.3 refusals |
| `power::dc_state_disable_keeps_the_bits_software_does_not_own` | §12.1's bits 9, 8, 4 and the status bit |
| `power::a_dc_state_that_will_not_take_the_write_is_an_error` | the 100-write retry bound |
| `power::a_well_whose_state_never_sets_names_every_cause_it_can` | §11 phase 1.3's diagnostic, with the requester comparison |
| `power::a_pg0_that_is_not_distributed_stops_the_well_before_the_request` | the fatal fuse poll, and the recorded one |
| `power::a_well_whose_state_never_sets_withdraws_the_request_it_added` | rollback at the handshake, and that a bit this call did not set survives |
| `power::a_failure_after_the_well_came_up_withdraws_it` | rollback for a later failure, with the original message carried through |
| `power::the_dbuf_reads_before_it_requests_and_reports_a_partial_part` | read-first, partial, and zero-slice outcomes |
| `power::the_error_mask_is_read_and_left_alone` | §3.7's decision |
| `power::a_workaround_register_that_drops_the_bits_is_an_error` | the `Wa_14011508470` read-back |
| `power::enabling_a_well_that_is_already_on_does_not_claim_credit` | the `already_on` distinction |
| `power::a_register_outside_the_window_is_an_error_rather_than_a_zero` | the error paths |
| `power::the_well_table_is_the_xe_lpd_one_and_not_tigers` | the indices, bits and fuse positions |
| `power::a_bring_up_that_finds_a_dead_cdclk_programs_one_and_says_so` | the whole sequence down the programming path |
| `regs::a_combo_phys_registers_are_at_the_documented_sub_block_offsets` | §5.1's correction, recomputed from the rule |
| `regs::the_writable_registers_are_exactly_the_ones_the_bring_up_programs` | a frozen write set |
| `regs::every_bring_up_register_is_inside_a_window_that_needs_no_forcewake` | the band and window invariants |
