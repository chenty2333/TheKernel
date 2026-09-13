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
(`afe_clock = clock * 1000 * 5`, `display/intel_dpll_mgr.c:1686`). **DisplayPort is not this**:
there the symbol rate is the link rate, not the pixel clock, which is why `pll.rs` has a general
`ddi_pll_dividers_for_symbol_rate` and the pixel-clock entry point is a thin wrapper over it.

### 3.3 The search — `[GAP]` §13.1 item 11, resolved

`[REF]` §13.1 item 11: *"I did not extract the exact loop, tolerance and tie-breaking rules from
`skl_ddi_calculate_wrpll`."* `pll.rs` implements `[I915]` `skl_ddi_calculate_wrpll`
(`display/intel_dpll_mgr.c:1660-1730`), including both of its tie-breaks:

* **Central frequencies and tolerance.** Three centres, `{8400, 9000, 9600} MHz`
  (`intel_dpll_mgr.c:1665-1667`). A candidate above its centre must be within **+1%**, one below
  within **−6%** (`SKL_DCO_MAX_PDEVIATION` = 100 and `SKL_DCO_MAX_NDEVIATION` = 600, in hundredths
  of a percent, `intel_dpll_mgr.c:1500-1502`). The asymmetry is i915's and is reproduced.
* **Candidate dividers.** An even list `{4, 6, …, 98}` and an odd list `{3, 5, 7, 9, 15, 21, 35}`
  (`intel_dpll_mgr.c:1668-1673`). The even list is not every even number: it is exactly those whose
  half decomposes under `skl_wrpll_get_multipliers`, and a test proves every entry does.
* **A deviation of zero ends the search** across all three centres (`intel_dpll_mgr.c:1703-1704`).
* **If any even divider was accepted, the odd list is never tried** — "If a solution is found with
  an even divider, prefer this one" (`intel_dpll_mgr.c:1709-1714`).

The last rule is strong enough to be worth a test of its own: at a 200 MHz symbol rate the even
divider 8 gives an 8000 MHz DCO, 4.76% below the 8400 MHz centre and therefore *accepted*, so the
odd divider 9 — which would land exactly on 9000 MHz — is never considered. i915 takes the worse
DCO. A well-meaning "fix" here would change which pixel clock the machine produces, so the behaviour
is pinned.

**The one place i915 and the PRM differ by construction.** `[REF]` §6.3 quotes the PRM's window
`DCO ∈ [7998, 10000] MHz` with a 8999 MHz midpoint, and notes the two framings are "compatible in
spirit but not bit-identical". They are not equivalent, and `pll.rs` exposes both so the difference
is visible rather than assumed: for a 395 MHz symbol rate i915 accepts the even divider 4 at
7900 MHz — outside the PRM's window — where the midpoint rule would take the divider 5 at 9875 MHz.
i915's rule is implemented because i915 is the only source here that targets display 13. The PRM
window is available as `DdiPllDividers::inside_prm_dco_window`.

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

The fraction is computed with exact integer arithmetic. `[I915]`'s C expression
(`intel_dpll_mgr.c:1654-1657`) divides `dco_freq` by `ref_clock / KHz(1)`, and with
`KHz(x) = 1000 * x`, `MHz(x) = 1000 * KHz(x)` (`i915_utils.h:336-337`) that is an **integer**
number of megahertz. For a 24 MHz reference it is exact; for 19.2 MHz it truncates to 19, which
makes the C expression disagree with the hardware equation the same file uses on read-back. This is
still true in mainline. `pll.rs` implements the exact form — the fraction is
`(DCO mod ref) * 0x8000 / ref` — which is what the read-back equation
`(integer + fraction / 0x8000) * ref` requires, and the difference is called out because anyone
comparing this module against i915's source will notice it.

### 3.5 The encoding gap — `[GAP]` §6.3, fenced, not resolved

`CFGCR1` holds `P` and `K` as small codes, and there are two mutually inconsistent accounts of what
those codes are.

* **`PllFieldEncoding::Executed`** — what i915 writes. `skl_wrpll_params_populate`
  (`display/intel_dpll_mgr.c:1611-1643`) codes `P` over `{1, 2, 3, 7} → {0, 1, 2, 4}` and `K` over
  `{5, 2, 3, 1} → {0, 1, 2, 3}`.
* **`PllFieldEncoding::Named`** — what i915's constants say. `i915_reg.h:4293-4296` defines
  `DPLL_CFGCR1_PDIV_{2,3,5,7}` as `{1,2,4,8} << 2` and `i915_reg.h:4287-4289` defines
  `DPLL_CFGCR1_KDIV_{1,2,3}` as `{1,2,4} << 6`, matching the PRM's `P ∈ {2,3,5,7}`,
  `K ∈ {1,2,3}`.

**Why they disagree, which the reference could only observe as a symptom.** The executed path's
candidate sets and codes are exactly the *Skylake* `DPLL_CFGCR2` convention — compare
`DPLL_CFGCR2_KDIV_{5,2,3,1} = {0,1,2,3} << 5` and `DPLL_CFGCR2_PDIV_{1,2,3,7} = {0,1,2,4} << 2` at
`i915_reg.h:4141-4150`. `skl_wrpll_params_populate` was written for that register and was reused
unchanged when `icl_calc_dpll_state` (`display/intel_dpll_mgr.c:2897-2900`) began shifting the same
values into the Gen12 `CFGCR1` positions. i915 then reads them back with the *named* convention in
`icl_ddi_combo_pll_get_freq` (`display/intel_dpll_mgr.c:1740-1802`). **i915's write and read
disagree, and both conventions are present in mainline today.**

The disagreement is total for `K`: the two code every one of `K ∈ {1,2,3}` differently. For `P` they
agree on 2 and 3 and differ on 7 (`4 << 2` against `8 << 2`, exactly as `[REF]` §6.3 records).

**What that costs, concretely.** For 1080p60 the chosen set is `(P=2, Q=3, K=2)`. Written with the
executed codes, `KDIV` gets 1. Read back with the named convention, `KDIV` = 1 means `K = 1`, so the
divisor is `5 × 2 × 3 × 1 = 30` instead of 60 and the read-back reports **297 MHz for a 148.5 MHz
request**. That is a factor-of-two error in a read-back path, which is why this document does not
pick a convention on i915's behalf.

**What `pll.rs` does about it.** Both conventions are offered as `PllFieldEncoding`, with no
`Default`, and the cases only one can express return a named `DividerNotEncodable` error:

* `P = 5` is reachable — a 360 MHz symbol rate puts the odd divider 5 exactly on the 9000 MHz centre
  with no even divider in tolerance — and has **no executed code at all**; i915 warns
  `"Incorrect PDiv"` and programs whatever its zero-initialised struct held. The named convention
  codes it `4 << 2`.
* `K = 5` is reachable — a 180 MHz symbol rate gives the even divider 10, which decomposes as
  `(2, 1, 5)` — and is **not a legal `K`** in the PRM's `K ∈ {1,2,3}` at all, so the named
  convention has no code for it. i915 writes `kdiv = 0`.

`[REF]` §6.3's route 1 remains the best answer and is unchanged: if the firmware has programmed a
combo DPLL for a mode you can use, read `CFGCR0`/`CFGCR1` and reuse the divider set verbatim. Route
2 — implement the executed encoding and verify by read-back — is available through
`PllRegisters::symbol_rate_hz`, which is a reimplementation of i915's read-back and round-trips
against whatever encoding it was written with.

### 3.6 `DPLL_CFGCR1[1:0]` is zero on this generation

`[REF]` §6.3's Gen12 delta: `CFSELOVRD` is
`TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL = 0 << 0` (`i915_reg.h:4299`), selected only for
`DISPLAY_VER >= 12` (`intel_dpll_mgr.c:2902-2905`). The ICL central-frequency selector that lives in
those bits on Gen11 does not apply. `cfgcr0` therefore needs no special handling for it; the field
is already correct at zero.

---

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
VIC 4 land on 8910 MHz with total dividers 12 and 24, DMT 0x04 on 9566.5 MHz with divider 76, which
is the case that exercises the asymmetric tolerance because it does *not* land on a centre. A sweep
of over 2,000 symbol rates across the legal range asserts the invariants — dividers multiply out to
the total, the total is a candidate the lists contain, the deviation is inside the documented limit,
the declared error matches the registers — and round-trips every set through its own decode. Every
sourced reference (19.2, 24, 38.4 MHz) is checked to give a self-consistent answer.

**Both.** The boundary cases are checked rather than assumed: a symbol rate below the smallest DCO
the PLL can make and above the largest both return an error, and so do zero rates and unsourced
references.

### 4.2 What the host tests do not check, and cannot

* **Nothing has been written to or read from silicon.** Not a register, not a divider, not a timing.
  Every number here is a claim about documentation and about arithmetic.
* **The `[GAP]` in §3.5 is not closed by a passing test.** The tests prove the two encodings differ
  and that the disputed cases are refused; they cannot say which one the hardware implements. Only a
  read-back on the machine can.
* **The DCO's real behaviour is not modelled.** The search places the DCO inside a documented window
  around a central frequency; whether the PLL locks there is a question for the machine.
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
