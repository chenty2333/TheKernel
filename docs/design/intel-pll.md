# Intel Gen12 display timings and DDI PLL arithmetic

**What this covers:** the arithmetic that turns a display mode into the numbers a Gen12 display
engine is programmed with — the transcoder timing registers, and the combo PHY (WRPLL) divider set
that produces the pixel clock. Nothing here touches hardware.

**Code:** `kernel/src/drm/intel/timing.rs` and `kernel/src/drm/intel/pll.rs`.
**Reference:** `docs/design/intel-display-registers.md` §5.2, §5.3, §6 and §11 phase 3.

**Status: nothing in either module has been run against silicon.** The evidence is a host test
suite over published modelines and over every row of the kernel's own DMT and CTA-861 tables. That
is strong evidence about the arithmetic and no evidence at all about the hardware. §13.4 of the
register reference — dump a working configuration's registers and diff — is still the next step,
and this document says at the end exactly which registers to diff.

## Why this is split out from the modeset

Both modules are pure functions over integers: a mode in, register values out; a pixel clock in,
dividers out. The MMIO stays in the modeset workstream that consumes the results. That split is not
tidiness — it is what makes the two things most likely to be wrong testable on a machine with no
display attached, which is every machine this work is being written on.

---

## 1. Provenance

The register reference's own markers are used unchanged. In this document they mean:

| Marker | Meaning |
|---|---|
| `[REF]` | `docs/design/intel-display-registers.md`, by section. Sourced there from Intel's Tiger Lake and DG1 PRMs. |
| `[I915]` | Linux v6.12 `drm/i915`, tag `v6.12`, commit `adc218676eef25575469234709c2d87185ca223a`. Cited by `file:line`. |
| `[INF]` | Inference from sourced facts. Labelled, never presented as sourced. |
| `[GAP]` | Could not be sourced. Listed again in §5. |

The register reference is built on Intel's Tiger Lake PRM plus the DG1 PRM as a cross-check, but
Tiger Lake is display IP version **12** and Alder Lake-N (`8086:46d0`) is display version **13**
(`XE_LPD`) — `[REF]` §0.2. There is no public ADL-N PRM, so for every display-13 delta the only
source in the set is i915. That is why two of `[REF]`'s open items are settled here from i915, and
why the one that i915 gets wrong is fenced rather than settled.

Facts are taken from i915; no code is copied from it, and every constant in the modules cites the
symbol it came from so the next reader can check it in one step.

---

## 2. Timings: a mode to registers

### 2.1 The `value − 1` convention

`[REF]` §6.1 gives the conversion, and §5.3 gives the field layout:

```
HTOTAL(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)
HBLANK(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)
HSYNC(T)   = ((hsync_end - 1) << 16) | (hsync_start - 1)
VTOTAL(T)  = ((vtotal  - 1) << 16) | (vdisplay - 1)
VBLANK(T)  = ((vtotal  - 1) << 16) | (vdisplay - 1)
VSYNC(T)   = ((vsync_end - 1) << 16) | (vsync_start - 1)
PIPESRC(T) = ((hdisplay - 1) << 16) | (vdisplay - 1)
```

**All fourteen halves of the six registers, and both halves of `PIPESRC`, store `count − 1`.** The
reference says it twice (§6.1 and §5.3) and repeats it at the point of use (§11 phase 3.4), which is
proportionate: this is the failure that does *not* look like a failure. A timing register that is
one too large produces a picture that rolls or sits off-centre — `[REF]` §11 phase 3.4, "A rolling
image means the timings are close but a total is off by one" — and a rolling picture sends the
reader to the sync generator, the memory path or the plane, none of which is where the bug is.

`HBLANK` and `VTOTAL`'s partner `VBLANK` hold the *same value* as `HTOTAL` and `VTOTAL`. That is not
a shortcut in the code: blanking runs from the end of the active region to the end of the line, so
the blank interval's "end" and "start" — which is what §5.3 calls `HBLANK`'s halves — are the line
total and the active width, which is exactly `HTOTAL`'s pair. §6.1 writes it that way outright.

### 2.2 How the module makes the convention impossible to get wrong

The decrement happens in exactly one function, `pack_minus_one`, which takes the two *counts* and
has no other way to be called. It uses checked subtraction, so a zero count is an error rather than
a wrap into `0xffff` — a value that would look entirely plausible in a register dump and describe a
65535-pixel line.

`TimingRegisters` hands back packed register values, and `unpack` is the inverse. Nothing in the API
exposes a count where a field value is wanted, so there is no second place a caller could subtract
one, and no place it could forget to.

### 2.3 What is deliberately not here

* **Sync polarity.** `[REF]` §6.1: "Polarity does **not** go here — it goes in `TRANS_DDI_FUNC_CTL`
  as `TRANS_DDI_PHSYNC` / `TRANS_DDI_PVSYNC`." A test pins that flipping a mode's polarity leaves
  all seven values unchanged.
* **Register offsets.** `regs.rs` owns the offset table and the rule that a named register must sit
  in a band that needs no forcewake; adding one there is that module's business, and it is a
  separate change. §5.3 gives the offsets for whoever does it: transcoder base `0x60000` for A
  (`0x61000`/`0x62000`/`0x63000` for B/C/D), `HTOTAL +0x00`, `HBLANK +0x04`, `HSYNC +0x08`,
  `VTOTAL +0x0c`, `VBLANK +0x10`, `VSYNC +0x14`, and `PIPESRC` at `0x6001c` (§5.2).
* **`TRANSCONF`'s interlace bit and `TRANS_VSYNCSHIFT`.** See below.
* **The pixel clock.** It is not a timing register; it comes from the port PLL, which is §3.
  `[REF]` §6.2: CDCLK only has to be fast enough, and the lowest ADL-N CDCLK (172.8 MHz) is ample
  for a single 1080p60 mode, so a first bring-up should pick the lowest and not change CDCLK.

### 2.4 Reduced blanking needs no special path

Reduced blanking changes the *totals* — a 1600x900 CVT-RB mode has 200 pixels of horizontal blank
and a one-line vertical front porch where the conventional timing has far more — and the totals are
all this conversion looks at. DMT 0x53 is tested as a row the table marks `reduced_blanking: true`,
so the claim is checked against a row that carries the flag rather than against one whose name
merely suggests it.

### 2.5 Interlaced is refused — `[GAP]`

`timing_registers` returns `TimingError::InterlaceNotSourced` for an interlaced mode. The six
registers would pack the same way, but they are not sufficient to program an interlaced mode, and
the reference does not source the rest. What is missing, and what is known about it:

* **`TRANSCONF`'s interlace field.** `[REF]` §5.2 lists `INTERLACE[23:21]` and gives no values.
  `[I915]` `i915_reg.h:1615` defines `TRANSCONF_INTERLACE_MASK_HSW = REG_GENMASK(22, 21)` — the
  Haswell-and-later mask, which is the one this generation uses — with
  `TRANSCONF_INTERLACE_W_SYNC_SHIFT` selected for an interlaced output
  (`[I915]` `display/intel_display.c:2963-2971`). **The reference's field position is therefore
  wrong by one bit for display version 13**, and the correct values live in a register §6.1 does not
  cover.
* **`TRANS_VSYNCSHIFT` (`+0x28`).** `[REF]` §5.3 says to leave it at reset "for a progressive RGB
  mode", which is how it says the register matters only for interlaced; it gives no value.
  `[I915]` computes `vsyncshift = crtc_hsync_start - crtc_htotal / 2`, adjusted by `+ crtc_htotal`
  if negative (`display/intel_display.c:2711-2717`).
* **The vertical total is not the mode's.** `[I915]` `intel_set_transcoder_timings`
  (`display/intel_display.c:2706-2709`) subtracts one *more* line from `VTOTAL` and `VBLANK_END` for
  interlaced, with the comment "the chip adds 2 halflines automatically". Following that faithfully
  needs to know what unit i915's `crtc_vtotal` is in for an interlaced mode — frame lines, field
  lines, or half-lines — and i915 does not settle it: `drm_mode_set_crtcinfo` is called with
  `CRTC_STEREO_DOUBLE` and not `CRTC_INTERLACE_HALVE_V`
  (`display/intel_display.c:4736-4737`), which establishes that nothing is halved but not what the
  hardware counts.

`[INF]` The most likely reading is that for interlaced the vertical fields count half-lines in a
field, which would make the active field `2 × vdisplay − 1` and explain both the "2 halflines" and
the extra `− 1`. This is stated as an inference and **not implemented**, because if it is wrong the
picture rolls and the person debugging it will be looking at the monitor, not at this file.

A wrong vertical total rolls the picture — precisely the failure §2.1's single-subtraction design
exists to prevent — so this refuses rather than guesses, and the error's documentation carries all
of the above so the next person starts from the citations. §5.5 says how to close it: read `VTOTAL`,
`VBLANK` and `TRANS_VSYNCSHIFT` from a firmware-programmed 1080i mode and diff.

---

## 3. The PLL

### 3.1 Structure on ADL-N

`[I915]` `adlp_plls[]` (`display/intel_dpll_mgr.c:4273-4283`) is the authority:

| PLL | id | Type | Registers | Used for |
|---|---|---|---|---|
| DPLL 0 | 0 | `combo_pll_funcs` | `DPLL0_*` | Combo PHY A — the one a first bring-up wants |
| DPLL 1 | 1 | `combo_pll_funcs` | `DPLL1_*` | Combo PHY B |
| TBT PLL | 2 | `tbt_pll_funcs` | `0x46020` | Type-C TBT |
| TC PLL 1–4 | 3–6 | `dkl_pll_funcs` | `PORTTC1/2_PLL_ENABLE` | Type-C ports |

`[REF]` §8.1: ADL-N's rear HDMI is on a combo PHY, and §6.3 concludes DPLL0 or DPLL1 is all a first
bring-up needs. `kernel/src/drm/intel/pll.rs` models exactly those two, as `ComboPhy::{A, B}`, and
offers no way to ask for a DKL PLL.

Register offsets, for the modeset workstream (`[I915]` `i915_reg.h:4301-4319`):

| Register | DPLL0 | DPLL1 |
|---|---|---|
| `CFGCR0` (DCO fraction and integer) | `0x164284` | `0x16428C` |
| `CFGCR1` (dividers) | `0x164288` | `0x164290` |
| `DIV0` (AFC startup) | `0x164B00` | `0x164C00` |
| `ENABLE` (= `LCPLL1_CTL` / `LCPLL2_CTL`) | `0x46010` | `0x46014` |

`[REF]` §6.3's warning stands: `_DPLL0_ENABLE` and `LCPLL1_CTL` are the same address, and there is
no separate register.

**Do not use `0x164000`/`0x164004` for `CFGCR0`/`CFGCR1`.** Those are `_ICL_DPLL0_CFGCR0`/
`_ICL_DPLL0_CFGCR1` (`[I915]` `i915_reg.h:4254,4275`), a *different* register pair. Gen12 parts use
the `_TGL_*` block at `0x164284`/`0x164288`. The *field* macros are shared, which is what makes this
easy to get wrong: the bit layouts in `[REF]` §6.3 are correct and `[I915]` agrees, but the base
address is not the ICL one.

### 3.2 The arithmetic

```
afe_clock = 5 * symbol_rate
DCO       = afe_clock * P * Q * K
symbol_rate = DCO / (P * Q * K) / 5
```

For HDMI the TMDS character rate equals the pixel clock, so `symbol_rate` is the pixel clock and
`afe_clock` is `5 × pixel clock` — which is what `[I915]` computes
(`afe_clock = crtc_state->port_clock * 5`, `display/intel_dpll_mgr.c:2784`). **DisplayPort is not
this**: there the symbol rate is the link rate, not the pixel clock, which is why `pll.rs` has a
general `ddi_pll_dividers_for_symbol_rate` and the pixel-clock entry point is a thin wrapper over
it.

### 3.3 The search — `icl_calc_wrpll`, and the Skylake search it replaced

`[REF]` §13.1 item 11 used to read *"I did not extract the exact loop, tolerance and tie-breaking
rules from `skl_ddi_calculate_wrpll`"*. That item is closed, and what closed it is not what the item
assumed: **ADL-N does not run `skl_ddi_calculate_wrpll` at all.** §6.3's corrections 1 and 2 record
it, and the loop this platform runs is `[I915]` `icl_calc_wrpll`
(`display/intel_dpll_mgr.c:2779-2820`), transcribed in §6.3 under "The search — now resolved from
the implementation".

`pll.rs` now implements that loop:

* **Bounds and aim point.** `dco_min = 7998000` kHz, `dco_max = 10000000` kHz, and
  `dco_mid = (dco_min + dco_max) / 2 = 8999 MHz` (`intel_dpll_mgr.c:2785-2787`). The PRM's three
  numbers and i915's three are the same three.
* **Candidate dividers.** A single flat list of 46 entries, the forty even ones — `{2, 4, 6, 8, 10,
  12, 14, 16, 18, 20, 24, …, 98, 100, 102}`, which is not every even number: 22, 26, 34, 38, 46,
  58, 62, 74, 82, 86 and 94 are absent — followed by `{3, 5, 7, 9, 15, 21}`
  (`intel_dpll_mgr.c:2788-2793`). It starts at 2 where the Skylake list starts at 4, and it stops at
  21, so the Skylake-only total divider 35 — the one with no PRM-legal `(P, Q, K)` — is gone.
* **The window is a hard bound.** A candidate whose DCO falls outside `[7998, 10000] MHz` is skipped
  before its distance is considered (`dco <= dco_max && dco >= dco_min`,
  `intel_dpll_mgr.c:2801`). A symbol rate with no in-window candidate is an error, not a nearest
  guess. This is the property the old search did not have.
* **Exhaustive, with strict `<`.** Unlike the Skylake loop there is no `min_deviation == 0` early
  exit: every candidate is tested, and the first entry in list order achieving the minimum distance
  from the midpoint wins (`dco_centrality < best_dco_centrality`, `intel_dpll_mgr.c:2804`, against a
  best initialised to `U32_MAX`). The evens are first in the list, so on a genuine tie an even
  divider wins — but only on a tie, not as the old search's blanket "an even divider beats every odd
  one" rule.
* **The decomposition is `icl_wrpll_get_multipliers`** (`intel_dpll_mgr.c:2507-2543`), with its
  branch order intact: `%4` before `%6` before `%5` before `%14`, so 20 is `(2, 5, 2)` and not
  `(5, 1, 2)`. Every branch yields `P ∈ {2,3,5,7}` and `K ∈ {1,2,3}` and sets `Q = 1` whenever
  `K != 2`, which is the PRM's own rule — so unlike the Skylake decomposition, no candidate the
  search can choose produces `K = 5`, and none is without a PRM-legal decomposition.

The module no longer carries the Skylake arithmetic at all. It lives on in one place, as the other
half of the measurement: a test-local implementation in
`output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`, which is what makes
the numbers in §3.7 reproducible and what would catch a revert.

### 3.4 Where the DCO comes from, and the reference clock

```
dco_integer  = DCO / ref
dco_fraction = frac(DCO / ref) * 0x8000
achieved DCO = (dco_integer + dco_fraction / 0x8000) * ref
```

Two facts about `ref` that the register reference does not record, both from `[I915]`:

* **A 38.4 MHz reference is divided to 19.2 MHz for this arithmetic.** `icl_wrpll_ref_clock`
  (`display/intel_dpll_mgr.c:1737-1752`): *"For ICL+, the spec states: if reference frequency is
  38.4, use 19.2 because the DPLL automatically divides that by 2."* `DCO_INTEGER` computed from
  the platform's 38.4 MHz instead of from 19.2 MHz is wrong by a factor of two — a black screen
  with no explanation.
* **At 38.4 MHz the programmed fraction is halved.** `ehl_combo_pll_div_frac_wa_needed`
  (`display/intel_dpll_mgr.c:2598-2605`) returns true for TGL, ADL-S, ADL-P and Elkhart Lake B0+
  when the strap is 38.4 MHz, with the comment "Program half of the nominal DCO divider fraction
  value" (`intel_dpll_mgr.c:2596`); `icl_calc_dpll_state` applies `DIV_ROUND_CLOSEST(fraction, 2)`
  (`intel_dpll_mgr.c:2891-2892`) and the read-back path doubles it again
  (`intel_dpll_mgr.c:2873-2874`). ADL-N is an ADL-P subplatform, so this applies to the target
  machine whenever its reference is 38.4 MHz. `pll.rs` models it as an explicit
  `DcoFractionWorkaround` parameter rather than assuming it, because it is a property of the
  platform and the divider arithmetic is not given one.

**Which reference ADL-N actually has is still open.** `[REF]` §13.1 item 4: the DG1 PRM says
38.4 MHz "not programmable", TGL and i915 say it is read from `SKL_DSSM` (`0x51004`, field
`[31:29]`: 0 = 24 MHz, 1 = 19.2 MHz, 2 = 38.4 MHz — `[I915]` `i915_reg.h:2880-2884`,
  `display/intel_cdclk.c:1583-1603`). `pll.rs` implements the decode, refuses the five undefined
  field values, and **the value must be read from `SKL_DSSM` on the machine** — §12.3 says the same.

The fraction is computed with exact integer arithmetic, and **the ADL-N encoder computes it the same
way**. `[I915]` `icl_wrpll_params_populate` (`display/intel_dpll_mgr.c:2588-2591`) does
`dco = div_u64((u64)dco_freq << 15, ref_freq)`, then `dco_integer = dco >> 15` and
`dco_fraction = dco & 0x7fff`, with both frequencies in kHz. `pll.rs` computes the integer from the
quotient and the fraction from the remainder, which is the same number; the
`the_dco_fraction_split_is_the_icl_encoders` test checks it against the C expression directly on
seven clocks at all three references.

**This paragraph used to describe a different function.** It cited
`intel_dpll_mgr.c:1654-1657`, which is the *Skylake* `skl_wrpll_params_populate`, and correctly
observed that *that* expression divides by a reference truncated to whole megahertz (`ref_clock /
KHz(1)`), so that for a 19.2 MHz reference it divides by 19. The observation was right; the frame
was wrong, in the same way §3.3's was: the `skl_` encoder is not on this path, so there is no
disagreement between i915 and `pll.rs` to call out here. It is the fourth instance of the mistake
`[REF]` §6.3's corrections list — reaching for a `skl_`-prefixed helper on an `XE_LPD` part — and it
was found while making the search change, by reading `icl_wrpll_params_populate` rather than the
function the paragraph named.

### 3.5 The two `CFGCR1` encoders, and which one this platform runs

`CFGCR1` holds `P` and `K` as small codes, and `pll.rs` offers **two** accounts of what those codes
are as `PllFieldEncoding`, with no `Default`:

* **`PllFieldEncoding::Named`** — the Gen12 constants, and what ADL-N's own encoder writes.
  `i915_reg.h:4293-4296` defines `DPLL_CFGCR1_PDIV_{2,3,5,7}` as `{1,2,4,8} << 2` and
  `i915_reg.h:4287-4289` defines `DPLL_CFGCR1_KDIV_{1,2,3}` as `{1,2,4} << 6`, matching the PRM's
  `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`. `icl_wrpll_params_populate`
  (`display/intel_dpll_mgr.c:2546-2570`) emits exactly those values, and the read path
  `icl_ddi_combo_pll_get_freq` decodes with the same constants.
* **`PllFieldEncoding::Executed`** — the codes `skl_wrpll_params_populate`
  (`display/intel_dpll_mgr.c:1611-1643`) writes: `P` over `{1, 2, 3, 7} → {0, 1, 2, 4}` and `K` over
  `{5, 2, 3, 1} → {0, 1, 2, 3}`, which are the *Skylake* `DPLL_CFGCR2` convention (compare
  `DPLL_CFGCR2_KDIV_{5,2,3,1} = {0,1,2,3} << 5` and `DPLL_CFGCR2_PDIV_{1,2,3,7} = {0,1,2,4} << 2` at
  `i915_reg.h:4141-4150`). **ADL-N does not call that function.**

**This section used to say something else, and the correction matters.** It said `Executed` was
"what i915 writes", that the two conventions were in conflict *on this platform*, and that i915's
write and read therefore disagree. `[REF]` §6.3's "the trap" and its §13.1 item 12 record that the
conflict was an artefact of comparing the Skylake encoder with the Gen12 decoder: ADL-N's chain is
`icl_calc_wrpll` → `icl_wrpll_get_multipliers` → `icl_wrpll_params_populate` →
`icl_calc_dpll_state` → `icl_dpll_write`, and `icl_wrpll_params_populate` emits exactly the named
values the decoder reads. **On this platform write and read round-trip under `Named`.** What is
genuinely dangerous is that both encoders write one `struct skl_wrpll_params`, so nothing in i915
stops one generation's encoder being handed to the other's decoder; that is the trap §6.3 describes,
and it is why this module still refuses to pick a convention for the caller.

The module's *behaviour* is unchanged by that correction: both encodings, no default, and
`DividerNotEncodable` for a divider set the requested encoding cannot express. Only the prose moved.

The disagreement between the two code sets is total for `K` — each codes every one of `K ∈ {1,2,3}`
differently — and for `P` they agree on 2 and 3 and differ on 7 (`4 << 2` against `8 << 2`). If a
caller writes with one and reads back with the other, the answer is wrong by a factor that depends
only on `K`: for 1080p60 `(P=2, Q=3, K=2)` written with the executed codes, `KDIV` gets 1, which the
named convention reads back as `K = 1`, so the divisor is `5 × 2 × 3 × 1 = 30` instead of 60 and the
read-back reports **297 MHz for a 148.5 MHz request**. The
`i915s_own_write_and_read_conventions_do_not_agree` test pins that, as a property of the two code
sets rather than as a claim about i915's behaviour on ADL-N.

**What `pll.rs` does about it.** Both conventions are offered, with no `Default`, and the cases only
one can express return a named `DividerNotEncodable` error:

* `P = 5` is reachable — a 360 MHz symbol rate puts the odd divider 5 on 9000 MHz, 1 MHz from the
  midpoint, with no other candidate in the window — and has **no code in the Skylake encoder**;
  i915's `skl_wrpll_params_populate` warns `"Incorrect PDiv"` and programs whatever its
  zero-initialised struct held, silently, as `P = 1`. ADL-N's encoder codes it `4 << 2` like any
  other `P`, so `Named` accepts it.
* `K = 5` is **not reachable any more.** It was, under the Skylake decomposition: a 180 MHz symbol
  rate gave the total divider 10, which `skl_wrpll_get_multipliers` decomposed as `(2, 1, 5)`. The
  ADL-N decomposition takes the `%5` branch for 10 and answers `(5, 1, 2)`, so `K = 5` no longer
  arises from any candidate — and it is not a legal `K` in the PRM's `K ∈ {1,2,3}` at all, so the
  named convention has no code for it. The guard is still tested; it is now unreachable rather than
  live.

`[REF]` §6.3's route 1 remains the best answer: if the firmware has programmed a combo DPLL for a
mode you can use, read `CFGCR0`/`CFGCR1` and reuse the divider set verbatim. Route 2 — mirror the
`icl_` pair and verify by read-back — is available through `PllRegisters::symbol_rate_hz`, which is a
reimplementation of i915's read-back and round-trips against whatever encoding it was written with.

### 3.6 `DPLL_CFGCR1[1:0]` is zero on this generation

`[REF]` §6.3's Gen12 delta: `CFSELOVRD` is
`TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL = 0 << 0` (`i915_reg.h:4299`), selected only for
`DISPLAY_VER >= 12` (`intel_dpll_mgr.c:2902-2905`). The ICL central-frequency selector that lives in
those bits on Gen11 does not apply. `cfgcr0` therefore needs no special handling for it; the field
is already correct at zero.

---

### 3.7 The defect this replaced, and the measurement

Kept as a record because the failure mode is the one this workstream exists to prevent, and because
three of the four corrections in `[REF]` §6.3 are the same mistake in different clothes: reaching
for a `skl_`-prefixed helper on an `XE_LPD` part.

**What was wrong.** `pll.rs` implemented `skl_ddi_calculate_wrpll`: three central frequencies
`{8400, 9000, 9600} MHz`, the asymmetric `+1%`/`−6%` tolerance, a `min_deviation == 0` early exit,
an even-divider list that won outright if anything in it was accepted, and an odd list ending in 35.
The module's own documentation described it as this platform's arithmetic, which is what made it a
defect rather than a documented guess.

**The measurement.** 985 symbol rates from 16 to 1000 MHz in 1 MHz steps, both searches run over the
same inputs (`output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`, which
still re-derives both sides):

| Count | Value |
|---|---|
| rates both searches could reach | 574 |
| of those, a different total divider | 114 |
| of those, the Skylake choice also outside the PRM's `[7998, 10000] MHz` window | 12 |
| rates where the Skylake choice was outside that window (all 12 are disagreements) | 12 |
| rates only the Skylake list could reach | 7 — 527–533 MHz, and **all 7 were outside the window too** |
| rates only the ADL-N list could reach | 245 |
| rates neither could reach | 159 |

Over the kernel's published progressive modes: 181 comparable, **56 with a different divider**, one
(CTA VIC 92) reachable only by the ADL-N list.

**Why it mattered.** A DCO outside `[7998, 10000] MHz` is a PLL that does not lock, and a PLL that
does not lock is a monitor reporting "no signal" — on a machine with no serial port and no second
display, the hardest failure here to diagnose. 12 of the 985 measured rates had that property, and
the target mode was not one of them, which is exactly why it could have survived first light and
appeared later.

**After the fix**, re-measured on the same grid: `pll.rs` and the reference transcription agree on
every one of the 819 rates either can reach, no returned divider is outside the window, and 166
rates are refused — the two gap bands between the candidate list's entries, `(500.000, 533.200) MHz`
(between 4 and 3) and `(666.666, 799.800) MHz` (between 3 and 2), the ends being exact: each is a
rate where the neighbouring divider lands exactly on a window bound. Those refusals are the documented
algorithm's, not this implementation's: a rate whose window falls entirely inside a gap in the
divider list has no legal divider set at all. **The fix does give up 7 rates the old search
served**, and every one of them was a rate where the old search programmed a DCO below 7998 MHz —
so nothing that used to lock stops locking.

**What is still inference.** That the ADL-N list, window and midpoint are what this silicon wants is
sourced from `[REF]` §6.3's transcription and cross-checked against i915 v6.12's
`icl_calc_wrpll`/`icl_wrpll_get_multipliers` in this session; it is *not* a hardware measurement.
The arithmetic now aims where Intel's own implementation aims, and §13.4's read-back is still what
would confirm that the PLL locks there.

## 4. Verification

### 4.1 What the host tests do check

```
cargo test --locked --manifest-path kernel/Cargo.toml --tests \
    --features bpf,perf-sampling,axtask/test \
    --target x86_64-unknown-linux-gnu -- drm::intel:: --test-threads=1
```

75 tests pass, 0 fail; 16 are `timing.rs`'s and 24 are `pll.rs`'s.

**Timings.** Each of the seven registers is checked against `[REF]` §6.1's formulas for CTA-861 VIC
16 (1920x1080@60) and VIC 4 (1280x720@60), VESA DMT 0x04 (640x480@60) and DMT 0x53 (1600x900@60,
reduced blanking). Then **every row of the kernel's DMT and CTA-861 tables is round-tripped**: 190
progressive modes have all seven register values unpacked and compared against the counts the mode
carried, with the halves in the order §6.1 names them; the 22 interlaced rows are asserted to be
refused. Flipping a mode's polarity or doubling its clock is asserted to leave the registers
unchanged.

**PLL.** Published modelines are checked against the divider the arithmetic must find — VIC 16 and
VIC 4 land on 8910 MHz with total dividers 12 and 24, DMT 0x04 on 9063 MHz with divider 72, which is
the case that exercises the midpoint rule because no candidate lands on 8999 MHz — and their total
dividers are pinned as a table, because four of the ten moved when the search changed. A sweep of
3,933 symbol rates from 17 to 1000 MHz in 250 kHz steps asserts the invariants on every answer:
dividers multiply out to the total, the total is a candidate the list contains, **the DCO is inside
the PRM's window**, the midpoint is the only aim point, the decomposition is the one the PRM's own
bounds would pick, the declared distance from the midpoint is the definition, and no other candidate
is closer — plus a round trip of every set through its own decode. The sweep also pins the 665 rates
in that range the candidate list cannot serve and where they are. Every sourced reference (19.2, 24,
38.4 MHz) is checked to give a self-consistent answer, and the `DCO_INTEGER`/`DCO_FRACTION` split is
checked against `icl_wrpll_params_populate`'s C expression directly.

**The two searches.** `output::tests::pll_rs_search_is_measured_against_the_documented_adl_n_search`
runs `pll.rs` against a re-derived transcription of §6.3's search over 985 symbol rates and every
published progressive mode, and asserts they agree everywhere (§3.7). It also carries the *Skylake*
search as test-local arithmetic and re-derives the pre-fix divergence from it, so the numbers in §3.7
stay reproducible and a revert would be caught rather than believed.

**Both.** The boundary cases are checked rather than assumed: a symbol rate below the smallest DCO
the PLL can make and above the largest both return an error, the exact rates at both ends are pinned
(15.683 MHz is the first the list can serve, 1000 MHz the last), and so are zero rates and unsourced
references.

### 4.2 What the host tests do not check, and cannot

* **Nothing has been written to or read from silicon.** Not a register, not a divider, not a timing.
  Every number here is a claim about documentation and about arithmetic. The search change in §3.3 is
  a change of algorithm, checked against the reference and against i915's source — it is not evidence
  that the PLL locks where the new arithmetic aims it.
* **The encoder question in §3.5 is not closed by a passing test.** The tests prove the two encodings
  differ and that the disputed cases are refused; they cannot say which one the hardware implements.
  Only a read-back on the machine can.
* **The DCO's real behaviour is not modelled.** The search places the DCO inside a documented window
  around a documented midpoint; whether the PLL locks there is a question for the machine.
* **The published modelines are checked against totals, not against a monitor.** A timing whose
  totals match a published table is a timing that is *described* correctly, which is a necessary and
  not a sufficient condition for a picture.
* **`timing.rs`'s round trip is a self-consistency check.** `unpack` is the inverse of
  `pack_minus_one` by construction, so the round trip would still pass if *both* were wrong in the
  same direction. What stops that is the handful of tests that pin the actual hexadecimal register
  values for four published modelines against `[REF]` §6.1's formulas — those are the ones to read
  if the convention is ever in doubt.

---

## 5. Gaps and inferences

### `[GAP]`

1. **Interlaced programming.** The `TRANSCONF` interlace field's values, `TRANS_VSYNCSHIFT`, and the
   unit of the extra line i915 removes from `VTOTAL`/`VBLANK_END`. Refused; see §2.5. `[REF]` §5.2's
   `INTERLACE[23:21]` is also wrong for this generation — the HSW+ mask is `[22:21]`.
2. **`PDIV`/`KDIV` encoding.** Two sourced conventions that disagree on every `K` and on `P = 7`.
   Fenced, not resolved; see §3.5. `[REF]` §13.1 item 7.
3. **The ADL-N PLL reference frequency.** 38.4 MHz (DG1 PRM, "not programmable") against
   `SKL_DSSM` (TGL PRM and i915). Decoded by `pll.rs`; must be read on the machine.
   `[REF]` §13.1 item 4.
4. **The ADL-N raw clock frequency.** Same shape of disagreement. Not in either module's scope.
   `[REF]` §13.1 item 5.
5. **Which PHY and port this SKU wires up.** `[REF]` §13.1 item 3, resolved by hotplug. `pll.rs`
   models combo PHY A and B only, on `[REF]` §8.1's statement that ADL-N's rear HDMI is on a combo
   PHY — a `[GFXINIT]` cross-check, not a per-SKU fact.

### `[INF]`

1. **The half-line reading of the interlaced vertical total** (§2.5). Consistent with both the
   "2 halflines" comment and the extra `− 1`, and not implemented because it is not sourced.
2. **That a ratio-perfect modeline is a good bring-up target.** `[REF]` §11 phase 3.1 already
   prefers 1080p60, and §3.3's VIC 16 result lands exactly on a 24 MHz reference with zero error;
   the inference that this makes it the *easiest* mode to bring up first is a judgement, not a
   sourced fact.
3. **That refusing `P = 5` and `K = 5` is better than choosing.** Once the encoding is settled by
   read-back, both cases have a defensible answer and the refusal should be replaced by it. Until
   then, the refusal is the only option that cannot produce a silently wrong clock.

---

## 6. What to read on the machine, first

`[REF]` §13.4's advice, narrowed to what would settle this document:

1. **`SKL_DSSM` (`0x51004`), field `[31:29]`.** Settles the reference frequency, and therefore
   whether the 38.4 → 19.2 division and the fraction workaround apply.
2. **`DPLL0_CFGCR0` (`0x164284`) and `DPLL0_CFGCR1` (`0x164288`)** from a mode the firmware
   programmed successfully — which on this machine means booting with the firmware's own output
   still up and dumping before anything clears it. Diff the `PDIV` and `KDIV` fields against both
   of `pll.rs`'s encodings for the same mode. Whichever matches is the convention, and §3.5 is
   closed.
3. **`HTOTAL`/`HBLANK`/`HSYNC`/`VTOTAL`/`VBLANK`/`VSYNC`/`PIPESRC`** (transcoder A: `0x60000`,
   `0x60004`, `0x60008`, `0x6000c`, `0x60010`, `0x60014`, `0x6001c`) for that same working mode.
   Every field should be one less than the published timing's count. This is the single check that
   settles §2.1 on real hardware, and it needs no monitor, no EDID and no modeset of our own.
4. **`VTOTAL`, `VBLANK` and `TRANS_VSYNCSHIFT` (`0x60028`) for an interlaced mode**, if the firmware
   can be made to program one. That is what §2.5 needs and nothing else will supply it.

None of these has been done.
