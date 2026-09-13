# Intel Gen12 / Xe-LP Display Registers — Implementation Reference

**Target hardware:** Intel Core i3-N305 (Alder Lake-N), integrated GPU, PCI `8086:46d0`.
**Goal:** bring the display engine up from scratch — no vendor driver, no firmware framebuffer —
until the kernel's own linear framebuffer is scanned out on a real monitor with correct timings.
**Scope of this document:** the *display* engine only. 3D (Mesa `iris`) is out of scope.

This document is written for someone who has never run code on this hardware. It is deliberately
front-loaded with the facts that cost the most time to get wrong: which BAR, which power well, which
register, which poll, which timeout.

---

## 0. How to read this document

### 0.1 Provenance markers

Every non-obvious claim carries a marker. A claim that cannot be traced is worse than no claim,
because a wrong register offset costs a hardware bring-up session.

| Marker | Meaning |
|---|---|
| `[TGL12]` | Read from Intel's **Tiger Lake PRM Volume 12: Display Engine** (`IHD-OS-TGL-Vol 12-12.21`, Rev 1.0, Dec 2021), retrieved in full in this session from Intel's own CDR host. Cited by its section name. **This is the closest public register-level documentation to Alder Lake-N** — same Xe-LP family, integrated, same power-well scheme. |
| `[TGL2C]` | Read from Intel's **Tiger Lake PRM Volume 2c: Command Reference — Registers, parts 1 and 2**, which carry the actual MMIO offsets and bitfields that Vol 12 refers to by name only. Both volumes are needed. |
| `[PRM]` | Read from Intel's **DG1 (Xe-LP, Gen12) Display Engine PRM** (`IHD-OS-DG1-Vol 12-2.21`, Feb 2021), retrieved in full in this session. Used as an *independent cross-check*: it agreed with the TGL PRM on every shared offset and table I tested. |
| `[I915]` | Read from the **Linux v6.12 `drm/i915`** tree (tag `v6.12`, commit `adc218676eef25575469234709c2d87185ca223a`), retrieved in this session. Cited by `file:line`. Facts only — never code or prose. |
| `[PRM]+[I915]` | The PRM(s) and i915 **agree**. Highest confidence. |
| `[GFXINIT]` | Read from coreboot's `libgfxinit` (AdaCore), retrieved in this session. An **independent** Gen12 implementation, used as a cross-check. |
| `[INF]` | **Inference** from verified facts. Labelled as such. Never presented as sourced. |
| `[GAP]` | Something important that I could **not** source. Listed again in §14. |

**Nothing in this document is from memory.** Every register offset, bitfield and table in §2 and in
the body was read out of a file or PDF fetched during this session. Where a value is
generation-specific and I am not certain it applies to Alder Lake-N, the text says so.

### 0.2 The single most important caveat

**No Alder Lake PRM is public.** I confirmed this by enumerating Intel's own PRM platform list:
12th-generation client is absent; the Gen12 entries stop at Tiger Lake, Rocket Lake, DG1, Lakefield
and Alchemist/ATS-M.

So this document is built from:

- **Intel's Tiger Lake PRM, Volumes 12 and 2c** — the closest public register-level documentation to
  ADL-N: same Xe-LP family, integrated part, same power-well scheme, and *one* generation closer
  than DG1. TGL is display IP version 12; Alder Lake-N is **display IP version 13** (`XE_LPD`).
- **Intel's DG1 PRM Volume 12 (Xe-LP display 12)** as an independent cross-check. It agreed with the
  TGL PRM on every shared offset, bitfield and table I tested.
- **Linux v6.12 `drm/i915`**, which is the *only* source in this set that actually targets display
  version 13.

`[I915]` `display/intel_display_device.c` — `xe_lpd_display` sets `.__runtime_defaults.ip.ver = 13`;
`adl_p_desc` uses `xe_lpd_display`, and `INTEL_ADLN_IDS` (which begins `0x46D0`) is a subplatform of
`adl_p_desc` at stepping D0.

The PRMs give the *architecture, names, offsets, bitfields and sequences*; i915 supplies the **Gen13
deltas**. Every place where the gap matters is called out inline as a **Gen13 delta**:

1. Power wells are a **tree** split per pipe (`PW_1`, `PW_2`, `PW_A`…`PW_D`) instead of TGL's
   sequential `PG0`…`PG5` chain. `[I915]` — and note that **Rocket Lake, also Gen12, has a third
   layout**, so this must be verified rather than inferred (§4.2.1).
2. `DBUF` is 4096 blocks across 4 slices with per-pipe slice configuration. `[I915]`
3. `has_cdclk_crawl` is set — CDCLK changes use a frequency-change request handshake. `[I915]`
4. `has_cdclk_squash` is **not** set (that arrives with Xe-HPD / `XE_LPDP`). `[I915]`
5. Four sprites plus primary and cursor per pipe (`num_sprites = 4` → planes 1–5 + cursor). `[I915]`
6. `PORT_TC5`/`PORT_TC6` are renumbered as `PORT_D_XELPD`/`PORT_E_XELPD`. `[I915]`
7. `DPLL_CFGCR1[1:0]` is `CFSELOVRD` (normal XTAL, 0) rather than ICL's central-frequency selector.
   `[I915]` + `[TGL12]`

**Every offset, bitfield and table in this document was read from one of the sources above during
the session that produced it. Nothing is from memory.** Where the PRMs and i915 disagree, §13.3 says
so and says which I trust and why.

### 0.3 What "verified" does not mean here

I have **not** run anything on the target machine. Nothing here has been validated against real
silicon. §13 lists the specific register reads that will settle each remaining question, and the
document is written so that those reads are the *next* action, not an afterthought.

---

## 1. Quick reference

### 1.1 Address map

| Item | Value | Source |
|---|---|---|
| PCI device | `8086:46d0`, iGPU (one function — see §3) | `[I915]` `include/drm/intel/pciids.h`, `INTEL_ADLN_IDS` |

> **`46d0` is not 1:1 with the i3-N305.** `INTEL_ADLN_IDS` is `0x46D0`–`0x46D4`; `[I915]` bug
> #10932 has a real **N200** reporting `46d0`. Use the device ID to identify the *platform*, and read
> `SKL_DFSM`/`DSSM` to identify the *configuration*. Do not hard-code i3-N305 behaviour on `46d0`.
>
> Also: there is **no GT2-class Alder Lake-N**. `[I915]` `gt/intel_sseu.c` states that TGL, RKL, DG1
> and ADL all have a single slice.
| MMIO BAR | **BAR 0** (`GTTMMADR`) | `[I915]` `intel_pci_config.h:19,25-32` |
| MMIO BAR size | **16 MiB** on Gen8+ | `[I915]` `gt/intel_ggtt.c:1134-1142` |
| Register window inside BAR 0 | **first 8 MiB** (offset `0x000000`–`0x7FFFFF`) | `[I915]` `gt/intel_ggtt.c:1144-1147` (`gttadr_offset = gttmmadr_size/2`) |
| GGTT array inside BAR 0 | **second 8 MiB** (offset `0x800000`) | same |
| GTT aperture BAR | **BAR 2** (`GMADR`) | `[I915]` `intel_pci_config.h:20` |
| Display register base | **offset 0** (no `DISPLAY_MMIO_BASE` bias on Gen12) | `[I915]` `display/intel_display_reg_defs.h:11` + `xe_lpd_display` leaves `mmio_offset` zero |
| Forcewake needed? | **No** anywhere in `0x40000`–`0x1BFFFF` (the display region), incl. combo PHY and DPLL config | `[I915]` `intel_uncore.c` `__gen12_fw_ranges[]`; see §2.1 |
| Stolen-memory / GTT base | `GEN6_GSMBASE` = `0x108100`, bits `[63:20]` | `[I915]` `i915_reg.h:4448,4451` |

### 1.2 Display register blocks (ADL-N, `XE_LPD`)

| Block | Base | Notes |
|---|---|---|
| Pipe A / B / C / D | `0x70000` / `0x71000` / `0x72000` / `0x73000` | `[I915]` `intel_display_device.c:56-59` |
| Transcoder A / B / C / D | `0x60000` / `0x61000` / `0x62000` / `0x63000` | `[I915]` `intel_display_device.c:73-77` |
| Cursor A / B / C / D | `0x70080` / `0x71080` / `0x72080` / `0x73080` | `[I915]` `intel_display_device.c:82-87,170-176` |
| Universal plane 1 (A) | `0x70180` | `[I915]` `skl_universal_plane_regs.h:31-37` |
| Universal plane 2 (A) | `0x70280` | same |
| Power wells (`PWR_WELL_CTL2`) | `0x45404` | `[I915]` `i915_reg.h:3627` |
| Power wells, AUX (`AUX2`) | `0x45444` | `[I915]` `i915_reg.h:3663` |
| Power wells, DDI IO (`DDI2`) | `0x45454` | `[I915]` `i915_reg.h:3691` |
| CDCLK | `0x46000` – `0x46070` | `[I915]` `i915_reg.h:4060,4093,4364` |
| Combo PHY A / B | `0x162000` / `0x06C000` | `[I915]` `intel_combo_phy_regs.h:11-12` |
| DDI buffer / AUX | `0x64000`+ (port A), `0x64100`+ (port B) | `[I915]` `i915_reg.h:3854-3855`, `intel_dp_aux_regs.h:24-25` |
| South display (GMBUS/HPD/PPS) | `0xC0000`+ | `[I915]` `i915_reg.h:2905` `PCH_DISPLAY_BASE` |
| DE interrupts | `0x44000`+ | `[I915]` `i915_reg.h:2457-2460` |

### 1.3 Register blocks you can ignore for a first light-up

Panel fitter, scalers, CSC/colour management, gamma LUTs, FBC, PSR, DSC/VDSC, DP MST, HDCP, VRR,
DSB, audio, DSI, wireless. A linear, unscaled, 8-bpc RGB scanout on one pipe needs **none** of them.
Resist the temptation to program them "correctly" before the first pixel appears.

---

## 2. Register access model

### 2.1 You do not need forcewake at all for display registers

This is worth stating early because most Intel display bring-up folklore is about forcewake
handshakes, and on Gen12 they are avoidable for everything in the display region.

**Two different mechanisms, and conflating them is the trap.** `[I915]` has both a *fast-path
filter* and an *authoritative range table*, and only the second one determines whether forcewake is
actually taken:

```
/* intel_uncore.c:909-913 -- a FILTER, not a requirement */
NEEDS_FORCE_WAKE(reg) = (reg < 0x40000 || reg >= 0x116000)

/* intel_uncore.c, __gen12_fw_ranges[] -- the AUTHORITY */
GEN_FW_RANGE(0x40000, 0x1bffff, 0)     /* domain 0 == no forcewake */
```

The filter only decides whether it is worth doing a table lookup. The domain it returns is what
matters:

```c
/* intel_uncore.c, fwtable_read32 / fwtable_write32 */
fw_engine = __fwtable_reg_read_fw_domains(uncore, offset);
if (fw_engine)                      /* <-- guarded on NON-ZERO */
        __force_wake_auto(uncore, fw_engine);
val = __raw_uncore_read32(uncore, reg);
```

and `__force_wake_auto` has `GEM_BUG_ON(!fw_domains)`, so the guard is load-bearing: a domain of
`0` means **nothing is acquired and a plain MMIO access is issued.**

**Therefore the whole range `0x40000`–`0x1BFFFF` requires no forcewake** — including the combo PHY
at `0x162000` and the DPLL configuration at `0x164xxx`. For those two:
`NEEDS_FORCE_WAKE(0x162000)` is *true* (they are `>= 0x116000`), so the code takes the slow path —
and `find_fw_domain()` then finds them inside `GEN_FW_RANGE(0x40000, 0x1bffff, 0)` and returns
**0**. No forcewake is taken. `[I915]` `intel_uncore.c:909-913` (filter), `:943-970`
(`find_fw_domain`), `:1900-1914` and `:1997-2002` (the guarded accessors), `:2433-2436`
(Gen12 selects `__gen12_fw_ranges` via `GRAPHICS_VER >= 12`, and ADL-P/N is `GEN12_FEATURES` →
`GEN(12)` per `i915_pci.c:634-640,685-692`).

The distinction that matters to an implementer:

| Range | Fast-path filter | Table domain | Forcewake needed? |
|---|---|---|---|
| `0x00000`–`0x3FFFF` | needs lookup | GT / render / media domains | **yes** |
| `0x40000`–`0x115FFF` | skipped (definitely safe) | — | **no** |
| `0x116000`–`0x1BFFFF` | needs lookup | **0 (none)** | **no** |
| `0x1C0000`+ | needs lookup | media VDBOX/VEBOX domains | **yes** |

So the practical rule for a display bring-up is simpler than the filter suggests: **everything in
the display region, from `0x40000` to `0x1BFFFF`, is safe without any forcewake implementation**,
and a from-scratch driver can skip forcewake entirely as long as it stays out of the GT range below
`0x40000` and the media range above `0x1C0000`.

> **This section previously contradicted §3.3 and the contradiction was resolved in favour of
> §3.3.** An earlier draft read the `0x116000` figure as "registers at or above this need
> forcewake", which would have swept in the combo PHY and DPLL configuration. That reading is
> wrong: `0x116000` is where the *filter* stops short-cutting, not where forcewake starts. Nothing
> in `0x40000`–`0x1BFFFF` needs forcewake. If you had already added a forcewake handshake
> specifically to reach `0x162000` or `0x164xxx` because of the older text, it was unnecessary —
> though harmless, and a useful thing to have written anyway if you later touch GT registers.

Remaining caveats, so this does not become a trap:

- Registers **below `0x40000`** genuinely need forcewake (VGA legacy, GT). Note `GEN6_GSMBASE`
  (`0x108100`) and `GGC` (`0x108040`) are **above** `0x40000`, so they are in the safe window too,
  despite sitting in the stolen-memory block.
- The safe-window claim is i915's model of the hardware, not a PRM statement. It is a strong
  signal, not a guarantee. §13 gives the read-back checks that catch a violation.
- Absent forcewake is not the only way a display register reads as zeros — **a powered-down well
  also reads as zero, and drops writes** (§4.2). If a register reads `0`, check the power well
  before suspecting forcewake.

### 2.2 Read-back discipline

Every write in this document is followed, in the real driver, by a read of the same register or by a
`posting read` of a neighbouring register. MMIO writes are posted. `[I915]` uses
`intel_de_posting_read()` after sequences where ordering matters. A from-scratch driver should do
the same: after a batch of writes, read one of them back before polling a status bit that the batch
was supposed to affect.

### 2.3 Polling helper semantics

`[I915]`'s `intel_wait_for_register(uncore, reg, mask, value, timeout_ms)`
(`intel_uncore.h:293-302`) spins for 2 µs and then sleeps, polling, for up to `timeout_ms`
**milliseconds**. All the "(timeout N)" figures below are in **milliseconds** unless explicitly
marked µs. `[I915]` `intel_uncore.c:2768-2798`.

The PRM's timeout figures are in microseconds and are much tighter than what i915 actually uses.
Both are recorded below.

---

## 3. Discovery

### 3.1 One PCI function, not two

On Alder Lake-N the display engine and the render/media engines are in the **same PCI function**.
There is no separate "display" PCI device as there is on some discrete parts and on the
`PCH`-attached generations.

`[I915]` enumerates a single integrated GPU device for `INTEL_ADLN_IDS`; `intel_mmio_bar()` returns
`GEN4_GTTMMADR_BAR` = 0 for every generation from 4 upward including Gen12
(`intel_pci_config.h:19,25-32`), and there is exactly one `drm_i915_private` with one `uncore` and
one MMIO mapping for the whole GPU. `[GFXINIT]` likewise brings up ADL-N as a single device.

`[INF]` Consequence for a from-scratch driver: you do **not** need to correlate two functions, and
there is no second BAR that "has the display registers". The display block is simply the low part of
BAR 0.

### 3.2 The BARs

| BAR | Name | Gen12 contents |
|---|---|---|
| 0 | `GTTMMADR` | 16 MiB. **First 8 MiB = registers**, second 8 MiB = GGTT page table array. |
| 2 | `GMADR` | Graphics aperture (the window that maps GGTT to system physical addresses for CPU access). On discrete/LMEM parts this BAR is *local memory* instead. |
| 4 | `IO` | Legacy I/O. Present but not needed. |

`[I915]` `gt/intel_ggtt.c:1134-1147`:

```
gen6_gttmmadr_size() = 4 MiB  (ver < 8)
                     = 16 MiB (ver >= 8)
gen6_gttadr_offset() = gttmmadr_size / 2
```

i915 only `ioremap`s the **first 2 MiB** for registers (`intel_uncore.c:2334-2358`: 2 MiB for
`GRAPHICS_VER >= 5` and `< IP_VER(12,70)` and not dgfx). **2 MiB is the register window, but the BAR
is 16 MiB.** A from-scratch driver must not assume `pci_resource_len(pdev, 0) == 2 MiB`; it must
check. The 2 MiB i915 value is a deliberate under-map, not the BAR size.

`[INF]` For a first bring-up, map the whole BAR 0 as uncached (or write-combining for the GTT half)
and use the low 2 MiB for registers. Do not map BAR 2 unless you intend CPU access to the aperture.

### 3.3 Display vs render/media, inside the window

The display engine occupies the **low** part of the register window. There is no single boundary
register; the practical boundaries on Gen12 are:

- `0x00000`–`0x3FFFF` — legacy/VGA, fuses, and GT (forcewake-gated).
- **`0x40000`–`0x1BFFFF` — the display / "non-GT" region.** This is the range the Gen12 forcewake
  table marks with domain `0`, i.e. **no forcewake required anywhere in it**. `[I915]`
  `intel_uncore.c`, `__gen12_fw_ranges[]` (`GEN_FW_RANGE(0x40000, 0x1bffff, 0)`). This includes the
  combo PHY at `0x162000` and the DPLL configuration at `0x164xxx`, which are above the
  `NEEDS_FORCE_WAKE` filter's `0x116000` threshold but still inside this domain-0 range — see §2.1
  for why the filter and the requirement are different things.
- `0x80000`–`0x8FFFF` — the **DMC's own MMIO window**. `[I915]` `display/intel_dmc_regs.h`
  (`DMC_MMIO_START_RANGE`/`END_RANGE`). This is *inside* the display region; it is **not** a
  display/GT boundary, which is a common misreading.
- `0xC0000`–`0xCFFFF` — "south display": GMBUS, hotplug, panel power, backlight.
- `0x162xxx`–`0x16Fxxx` — combo PHY, DPLL configuration, `DPCLKA`. Inside the domain-0 range above,
  so **no forcewake despite the high address**.
- `0x1C0000`+ — high media engines (VDBOX/VEBOX). **This is where forcewake genuinely resumes.**

`[I915]` `i915_reg.h:2905` defines `PCH_DISPLAY_BASE = 0xc0000u`, and
`display/intel_gmbus.c:871-879` sets the GMBUS base to it for every platform that is not Valleyview
and not GMCH — which includes ADL-N. So the "south display" window on a PCH-less part is still at
`0xC0000`.

`[I915]` `i915_reg.h:2470-2476` shows the top-level display interrupt tree, which is the cleanest
"is this display?" test: `DEISR` at `0x44000` carries `GEN8_DE_MISC_IRQ`, `GEN8_DE_PORT_IRQ` and
`GEN8_DE_PIPE_{A,B,C}_IRQ`.

**A note on the two apertures.** Do not confuse the *register region* boundary above with the *BAR*
boundary in §3.2. BAR 0 is 16 MiB and is split at **8 MiB** into registers (low) and the GGTT array
(high); that 8 MiB split is a BAR-layout fact. The `0x40000` figure is a register-address fact about
which block a given offset belongs to. They are unrelated numbers that happen to look similar.

### 3.4 How to tell the display is present at all

Read `SKL_DFSM` (`0x51000`) and check the pipe-disable fuse bits. `[I915]`
`display/intel_display_device.c:1646-1672`:

| Bit | Mask | Meaning if set |
|---|---|---|
| `SKL_DFSM_PIPE_A_DISABLE` | `1<<30` | Pipe A fused off |
| `SKL_DFSM_PIPE_C_DISABLE` | `1<<28` | Pipe C fused off |
| `TGL_DFSM_PIPE_D_DISABLE` | `1<<22` | Pipe D fused off (Gen12+) |
| `SKL_DFSM_PIPE_B_DISABLE` | `1<<21` | Pipe B fused off |
| `SKL_DFSM_DISPLAY_PM_DISABLE` | `1<<27` | FBC unavailable |
| `SKL_DFSM_DISPLAY_HDCP_DISABLE` | `1<<25` | HDCP unavailable |
| `GLK_DFSM_DISPLAY_DSC_DISABLE` | `1<<7` | DSC unavailable |
| `ICL_DFSM_DMC_DISABLE` | `1<<23` | DMC unavailable |

If masking off the fused-off pipes leaves an empty pipe mask, the display is fused off entirely
(`[I915]` `intel_display_device.c:1674-1675`).

**Read this register first.** It is one read, it needs no power well, and it tells you which pipes
you are even allowed to use. `[GAP]` I found no public statement of which pipes a given i3-N305 SKU
fuses off; assume all four are present and verify with this read.

### 3.5 Fused ports

`SFUSE_STRAP` (`0xC2014`) carries per-DDI presence and the raw-clock strap.
`[I915]` `i915_reg.h:4391-4399`:

| Bit | Name |
|---|---|
| 13 | `SFUSE_STRAP_FUSE_LOCK` |
| 8 | `SFUSE_STRAP_RAW_FREQUENCY` |
| 7 | `SFUSE_STRAP_DISPLAY_DISABLED` |
| 6 | `SFUSE_STRAP_CRT_DISABLED` |
| 3..0 | `SFUSE_STRAP_DDIF/D/E/C/B/D_DETECTED` (bit 3 = F, 2 = B, 1 = C, 0 = D) |

`[INF]` These `*_DETECTED` bits are a legacy signal and i915 no longer uses them to decide port
presence on Gen12 (it uses VBT/OpRegion child devices). Treat them as *hints* to print in a debug
log, not as the authority. The authority is the VBT (§3.6) plus the live hotplug state (§11).

### 3.6 VBT / OpRegion / pcode — what you can skip

i915 reads a **VBT** (Video BIOS Table) out of the ACPI OpRegion to learn port presence, panel
timings, backlight PWM frequency, lane counts and `aux_ch` mapping. `[I915]`
`display/intel_bios.c`, `display/intel_opregion.c`.

For a first light-up on an **external HDMI monitor** you can skip all of it: do not read the VBT,
do not parse the OpRegion, and drive the output from the monitor's own EDID. This is a deliberate
simplification and it is the single biggest scope reduction available.

You **cannot** skip it if the target is the internal eDP panel, because panel power sequencing
delays and backlight PWM frequency live there. `[I915]` `display/intel_pps.c` and
`display/intel_panel.c`. Plan the first bring-up on HDMI.

**Important structural fact if you do need it.** `[I915]` derives port presence from the VBT, not
from a fuse: `port_strap_detected()` begins `/* straps not used on skl+ */ if (DISPLAY_VER >= 9)
return true;`, and `intel_setup_outputs()` enumerates encoders by iterating the VBT's child devices
(`intel_bios_for_each_encoder` → `intel_bios_is_port_present`). So on ADL-N **the VBT is the only
authority on which ports and panel exist**, and reading a fuse instead will not work. If i915 finds
no VBT it does not fail cleanly — it hits a `WARN_ON` in `intel_bios_is_port_present()` and returns
a degenerate answer.

Where the VBT lives, for reference:
- OpRegion is located via PCI config **`ASLS = 0xFC`**, is **8 KiB**, and begins with the 16-byte
  signature `"IntelGraphicsMem"`. Layout: header `0x000`, mailbox #1 `0x100`, #2 `0x200`,
  #3 (ASLE) `0x300`, **#4 (VBT) `0x400`**, #5 `0x1C00`. `[I915]` `intel_pci_config.h`,
  `display/intel_opregion.c`.
- Mailbox #4 is a fixed **6 KiB** VBT buffer. The ASLE mailbox also carries `RVDA`/`RVDS` (raw VBT
  address and size) as an alternative source.
- OpRegion header has a `pcon` field whose **`PCON_HEADLESS_SKU` bit (bit 13)** lets firmware declare
  the SKU headless; i915 honours it (requires OpRegion version ≥ 2.3) and disables display entirely.
  `[I915]` `intel_opregion_headless_sku()`, `intel_display_device_enabled()`. **Check this bit
  before concluding the hardware is broken.**
- coreboot builds **OpRegion version 2.1** for Alder Lake
  (`src/soc/intel/alderlake/Kconfig` selects `INTEL_GMA_OPREGION_2_1`).

`[GAP]` There is **no public VBT format specification** and no Gen12-era OpRegion specification
(the public OpRegion spec is Skylake Rev 0.5, 2016). For the VBT, the practical references are
`[I915]`'s `display/intel_vbt_defs.h` and the `intel_vbt_decode` tool.

`[GAP]` Whether the i3-N305 machine in front of you has an eDP panel at all is unknown to me and
unknowable from documents. §13 gives the probe.

---

## 4. Power

This is where first bring-ups die. Read the whole section before writing any code.

### 4.1 The power well tree

On `XE_LPD` (ADL-N) the display power wells form a **tree**, not a chain. `[I915]`
`display/intel_display_power_map.c:1196-1213` draws it:

```
        PG0   (always on, "DC_off" well)
         |
       PG1 / PW_1
       /       \
    PW_A       PG2 / PW_2
               /   |   \
            PW_B  PW_C  PW_D
```

The rule that comes with the tree, verbatim in meaning (`[I915]` same comment block):
**power wells must be enabled top to bottom, and disabled bottom to top.** This is what allows
pipes to be power-gated independently on `XE_LPD` — a change from earlier generations where the
chain `PG(n-1) → PG(n)` was strictly linear.

For a **single-pipe** first bring-up the wells you need are: `PG0` (already on), `PW_1`, `PW_2`,
`PW_A`, plus the DDI-IO and AUX wells for whichever port you use.

### 4.2 Power well registers

All in the "driver" request register `HSW_PWR_WELL_CTL2 = 0x45404` for the main wells, and in
`ICL_PWR_WELL_CTL_AUX2 = 0x45444` / `ICL_PWR_WELL_CTL_DDI2 = 0x45454` for the AUX and DDI-IO wells.
`[I915]` `i915_reg.h:3626-3700`.

Bit layout inside a well-control register, per well index `i`:

```
REQ(i)   = 0x2 << (i*2)      /* request: I want this well on   */
STATE(i) = 0x1 << (i*2)      /* state:   the well is actually on */
```

`[I915]` `i915_reg.h:3630-3631`.

> **When a power well is down, writes to its registers are DROPPED and reads return ZERO.**
> `[TGL12]` states this explicitly. It is the reason a wrong power-well sequence presents as
> "every register reads 0", which looks like a dead device rather than a power bug. If a whole
> block reads as zeros, **suspect the power well before suspecting the block.**

### 4.2.1 Power-well maps are per-project — do not generalise

Three Gen12 projects, three different maps. `[TGL12]` + `[I915]`:

| Project | Display IP | Scheme |
|---|---|---|
| Tiger Lake, DG1 | 12 | **Chain** `PG0→PG1→PG2→PG3→PG4→PG5`, sequential, **no skipping**. `PWR_WELL_CTL` bits: PG5 = 9/8, PG4 = 7/6, PG3 = 5/4, PG2 = 3/2, PG1 = 1/0. |
| Rocket Lake | 12 | Different again: PG4 = 7/6, PG3 = 5/4, **bits 3:2 reserved (no PG2)**, PG1 = 1/0, **no PG5**. |
| **Alder Lake-P/N (`XE_LPD`)** | **13** | **Tree**, not chain. `PW_1` = idx 0, `PW_2` = idx 1, `PW_A..PW_D` = idx 5..8 (table above). |

`[TGL12]` also notes that PG2–PG5 each require their own `FUSE_STATUS` distribution-status poll
after the state bit, with a 20 µs timeout, and that the enable order is strictly sequential.

**For ADL-N, use the `XE_LPD` tree table in §4.2 — not the TGL chain.** The TGL chain is included
here because it is what the public PRMs document, and because the fact that TGL and RKL disagree
*with each other* is the strongest available argument that the ADL-N map must be verified on
hardware rather than inferred. §13 gives the verification.

There are four *request* registers, one per requester, and they are OR-ed by hardware. `[I915]`
`i915_reg.h:3616-3625`:

| Register | Requester |
|---|---|
| `HSW_PWR_WELL_CTL1` `0x45400` | BIOS |
| `HSW_PWR_WELL_CTL2` `0x45404` | **driver (use this one)** |
| `HSW_PWR_WELL_CTL3` `0x45408` | KVMR |
| `HSW_PWR_WELL_CTL4` `0x4540C` | debug |

`[PRM]` "Sequences to Initialize Display" step 3b says the same thing: *"There are two sets of
PWR_WELL_CTL registers for software use. It is expected that BIOS uses PWR_WELL_CTL1 and driver
uses PWR_WELL_CTL2."* `[PRM]+[I915]` agree.

**Well indices for `XE_LPD`** (`[I915]` `i915_reg.h:3650-3655`, `3674-3698`):

| Well | Register | Index | REQ bit | STATE bit |
|---|---|---|---|---|
| `PW_1` | `0x45404` | 0 | `0x2` | `0x1` |
| `PW_2` | `0x45404` | 1 | `0x8` | `0x4` |
| `PW_A` | `0x45404` | 5 | `0x800` | `0x400` |
| `PW_B` | `0x45404` | 6 | `0x2000` | `0x1000` |
| `PW_C` | `0x45404` | 7 | `0x8000` | `0x4000` |
| `PW_D` | `0x45404` | 8 | `0x20000` | `0x10000` |
| `DDI_IO_A` | `0x45454` | 0 | `0x2` | `0x1` |
| `DDI_IO_B` | `0x45454` | 1 | `0x8` | `0x4` |
| `DDI_IO_TC1` | `0x45454` | 3 | `0x80` | `0x40` |
| `AUX_A` | `0x45444` | 0 | `0x2` | `0x1` |
| `AUX_B` | `0x45444` | 1 | `0x8` | `0x4` |
| `AUX_USBC1` | `0x45444` | 3 | `0x80` | `0x40` |

`[GAP]` I could not confirm from any source which of `DDI_IO_C`…`DDI_IO_E` and `AUX_C`…`AUX_E` are
physically present on ADL-N. `[I915]` declares them (`xelpd_power_wells_main`, `intel_display_power_map.c:1332-1385`)
but that describes the *maximum* `XE_LPD` configuration, not this SKU. Probe per §13.

### 4.3 What each well powers

`[I915]` `display/intel_display_power_map.c:1215-1240`, `1288-1300`:

- **`PW_A`** → `POWER_DOMAIN_PIPE_A`, `PIPE_PANEL_FITTER_A`. **`PW_B`/`PW_C`/`PW_D`** → the matching
  pipe + its panel fitter + its transcoder.
- **`PW_2`** → all of `PW_B`, `PW_C`, `PW_D`'s domains **plus** the entire south-display port set:
  all `PORT_DDI_LANES_*`, `VGA`, `AUDIO_PLAYBACK`, `AUX_IO_*`, `AUX_C`…`AUX_E`, `AUX_USBC1`…`4`,
  `AUX_TBT1`…`4`.
- **`PW_1`** (and PG0) → under hardware/DMC control: DBUF function, **Transcoder A**, **DDI_A and
  DDI_B**, PCI, all clocks except the port PLLs, interrupts, MBus (except `PIPE_MBUS_DBOX_CTL`),
  DBUF registers, central power except FBC, and the top-level GTC.

`[INF]` **The most useful consequence:** a first bring-up on **pipe A + DDI A + transcoder A** is
the cheapest possible configuration, because transcoder A and DDI A are already inside `PW_1`,
which you must enable anyway. Choosing pipe B or C instead pulls in `PW_2` and an extra well for no
benefit. **Use pipe A.**

### 4.4 The well enable handshake

`[I915]` `display/intel_display_power_well.c:342-384`, `259-286`:

```
enable(well):
    if well.has_fuses:
        pg = ICL_PW_CTL_IDX_TO_PG(idx)          # PG1 for PW_1, PG2 for PW_2, ...
        if pg == PG1:
            wait for FUSE_STATUS.PG0_DIST_STATUS == 1
    rmw(PWR_WELL_CTL2, clear=0, set=REQ(idx))
    poll PWR_WELL_CTL2.STATE(idx) == 1
    if well.has_fuses:
        wait for FUSE_STATUS.PG_DIST_STATUS(pg) == 1
```

Notes that matter:

- `ICL_PW_CTL_IDX_TO_PG(idx) = idx - ICL_PW_CTL_IDX_PW_1 + SKL_PG1`, i.e. `PW_1 → PG1`,
  `PW_2 → PG2`. **All of `PW_A`…`PW_D` carry `.has_fuses = true` as well**
  (`[I915]` `intel_display_power_map.c:1329,1337,1345,1353`), so each one has a fuse poll after its
  state bit. Because `ICL_PW_CTL_IDX_TO_PG(idx) = idx - 0 + SKL_PG1`, that poll is for
  `SKL_FUSE_PG_DIST_STATUS(pg)` with `pg = idx + 1` — i.e. **PG6…PG9 for indices 5…8**, which are
  bits `27 - 6 = 21` down to `27 - 9 = 18` of `SKL_FUSE_STATUS`. Note those `PG6+` bit positions are
  **outside** the PG0…PG5 range that `[TGL12]` documents, which is further evidence that the
  `XE_LPD` well map is not the TGL map. `[INF]` on the PG6…PG9 naming; the arithmetic follows from
  `[I915]` `i915_reg.h:3735-3738` and `display/intel_display_power_well.c:371-378`.
- The fuse register is `SKL_FUSE_STATUS = 0x42000`. `SKL_FUSE_PG_DIST_STATUS(pg) = 1 << (27 - pg)`
  with `SKL_PG0=0, SKL_PG1=1, SKL_PG2=2`. `[I915]` `i915_reg.h:3724-3745`.
- `[PRM]` "Initialize Sequence" step 3 gives the timeouts: **20 µs** for the PG0 fuse, **30 µs**
  (38.4 MHz ref) or **45 µs** (24 MHz ref) for the PG1 state bit, **20 µs** for the PG1 fuse.
  `[I915]` `gen9_wait_for_power_well_fuses` passes i915's generic 1 ms.
- `[I915]` `hsw_wait_for_power_well_enable` uses `enable_timeout ?: 1` ms, but the `AUX_USBC*` wells
  on ADL-P get `enable_timeout = 500` (ms) for `WA_14017248603: adlp`.
  `[I915]` `intel_display_power_map.c:1394-1399`, `intel_display_power_well.c:262-286`.
- **Disable does not require polling the STATE bit.** `[I915]`
  `hsw_wait_for_power_well_disable` (`intel_display_power_well.c:304-328`) polls only for
  paranoia and then gives up if another requester still holds the well, logging which requester.
  `[PRM]` agrees: *"Wait for 10us. Do not poll for the power well to disable. Other clients may be
  keeping it enabled."*

### 4.5 Two ADL-specific workarounds you must not skip

1. **`Wa_16013190616: adlp`** — when enabling the power well whose PG is **PG1**, set
   `DISABLE_FLR_SRC` (bit 15) in `GEN8_CHICKEN_DCPR_1` (`0x46430`) **before** the request write.
   `[I915]` `display/intel_display_power_well.c:346-355`, register at `i915_reg.h:2838,2845`.
   `IS_ALDERLAKE_P` is true for ADL-N (it is a *subplatform* of ADL-P in i915), so this applies to
   the target. `[I915]` `intel_display_device.c:1100-1112`.

2. **`Wa_14011508470: tgl,dg1,rkl,adl-s,adl-p,dg2`** — after CDCLK and DBUF are up, set
   `DCPR_CLEAR_MEMSTAT_DIS | DCPR_SEND_RESP_IMM | DCPR_MASK_LPMODE |
   DCPR_MASK_MAXLATENCY_MEMUP_CLR` in `GEN11_CHICKEN_DCPR_2` (`0x46434`).
   `[I915]` `display/intel_display_power.c` `icl_display_core_init`, register at
   `i915_reg.h:2849-2853`.

And one that does **not** apply, which is worth knowing so you do not add it:

3. `adlp_cmtg_clock_gating_wa` (`TRANS_CMTG_CHICKEN` / `DISABLE_DPT_CLK_GATING`) is gated on
   `IS_DISPLAY_STEP(i915, STEP_A0, STEP_B0)`. ADL-N's stepping table
   (`adl_p_adl_n_steppings`) maps only `0x0 → STEP_D0`, so **ADL-N never runs it**.
   `[I915]` `display/intel_dpll_mgr.c:3927-3940`, `intel_display_device.c:1088-1090`.

### 4.6 CDCLK

#### What CDCLK is

CDCLK is the core display clock. It clocks the pipes, the DBUF and most of the display engine. The
DPLLs and the DDI clocks are *separate* and are covered in §8. The PRM states the constraint you
must respect: **CDCLK must be at least twice the Azalia BCLK** and it bounds the maximum pixel rate
and the watermark values. `[PRM]` "Restrictions", "Sequences for Changing CD Clock Frequency".

#### The registers

| Register | Offset | Key fields |
|---|---|---|
| `CDCLK_CTL` | `0x46000` | `CDCLK_FREQ_SEL[27:26]`, `MDCLK_SOURCE_SEL[25]`, `CD2X_DIV_SEL[23:22]`, `CD2X_PIPE[21:19]`, `CDCLK_FREQ_DECIMAL[10:0]` |
| `BXT_DE_PLL_ENABLE` — a.k.a. **`CDCLK_PLL_ENABLE`** | `0x46070` | `PLL_ENABLE[31]`, `LOCK[30]`, `SLOW_CLK_ENABLE[27]`, `SLOW_CLK_LOCK[26]`, `FREQ_REQ[23]`, `FREQ_REQ_ACK[22]`, `RATIO[7:0]` |
| `CDCLK_SQUASH_CTL` | `0x46008` | **not present/used on `XE_LPD`** — `has_cdclk_squash` is unset for `xe_lpd_display` |

`[I915]` `i915_reg.h:4060-4090`, `4364-4373`; `[TGL12]` Clocks → CDCLK. Squash absence from
`intel_display_device.c:1073-1085` (`xe_lpd_display` sets `has_cdclk_crawl` but not
`has_cdclk_squash`) versus `xe_hpd_display` / `XE_LPDP_FEATURES` which do set it.

`[TGL12]` names `0x46070` `CDCLK_PLL_ENABLE` and says *"Programming is done through the
CDCLK_PLL_ENABLE register"* with the ratio in it. `[PRM]+[I915]` agree on the address and on the
ratio-in-enable-register model for Gen11+.

**Note on bits 27/26.** `[TGL12]` calls `0x46070[27]`/`[26]` *Slow Clock Enable / Slow Clock Lock*,
whereas i915's `PLL_POWER_ENABLE`/`PLL_POWER_STATE` names for bits 27/26 belong to the **combo DPLL**
registers `0x46010`/`0x46014`, not to `0x46070`. These are different bits on different registers
that happen to share a bit number. Do not carry the combo-DPLL power-up step into the CDCLK PLL
sequence — that was an error in an earlier draft of this document.

#### `CDCLK_CTL` field encodings

`[TGL12]` Clocks → CDCLK, cross-checked against `[I915]` `i915_reg.h:4069-4081`:

| Field | Encoding |
|---|---|
| `CD2X_DIV_SEL[23:22]` | `00b` = ÷1, `10b` = ÷2 on TGL/DG1. RKL adds `01b` = ÷1.5 and `11b` = ÷4. i915 names all four (`BXT_CDCLK_CD2X_DIV_SEL_{1,1_5,2,4}`), so **ADL-N very likely has all four** — but TGL/DG1's PRM only lists two. `[INF]` |
| `CD2X_PIPE[21:19]` | `000` = A, `010` = B, `100` = C, `110` = D, `111` = none |
| `CDCLK_FREQ_DECIMAL[10:0]` | **U10.1 format**: `(frequency in MHz rounded to the nearest 0.5) − 1`, times 2 |

`[TGL12]`'s `CD2X_PIPE` encoding and i915's `TGL_CDCLK_CD2X_PIPE(pipe) = pipe << 20`
(`i915_reg.h:4079`) agree exactly: A→0, B→bit20, C→bit21, D→bits21:20, none→`7 << 19`.

The `CDCLK_FREQ_DECIMAL` semantic is worth stating because it lets you check the two sources
against each other, and **they agree**:

- `[TGL12]`: U10.1 of `round_to_0.5MHz(f) − 1`.
- `[I915]` `skl_cdclk_decimal(cdclk) = DIV_ROUND_CLOSEST(cdclk - 1000, 500)` (`intel_cdclk.c:1024-1027`).

For 307.2 MHz: PRM → round 307.2 to 307.0, minus 1 = 306.0, U10.1 = **612**.
i915 → round(306200/500) = round(612.4) = **612**. ✓
For 172.8 MHz: PRM → 173.0 − 1 = 172.0, U10.1 = **344**. i915 → round(171800/500) = round(343.6) =
**344**. ✓

#### The reference clock — **this is a real source disagreement, and it matters**

- `[PRM]` (DG1): *"Display Engine Clock Reference … 38.4 MHz Non-SSC … Default after reset:
  Enabled … Programming: **Not programmable by display software**."* And the CDCLK ratio table is
  given *for a 38.4 MHz reference*: `ratio 9→172.8, 10→192, 16→307.2, 34 (CD2X÷2)→326.4, 29→556.8,
  34→652.8` MHz.
- `[I915]` (Gen12 integrated): the reference is **read from hardware**, not fixed.
  `icl_readout_refclk` reads `SKL_DSSM` (`0x51004`), mask `ICL_DSSM_CDCLK_PLL_REFCLK_MASK = 7<<29`
  (`[I915]` `i915_reg.h:2880-2884`), decoding to **24 MHz** (0), **19.2 MHz** (1), **38.4 MHz** (2).
  `[I915]` `display/intel_cdclk.c:1583-1603`.

**Which to trust:** `[I915]`, because it targets the same class of silicon as ADL-N and because the
PRM explicitly says the reference is a *hardware* input the driver only reads. The PRM's DG1 table
is simply the special case `ref = 38.4 MHz`.

`[INF]` Why the difference exists: DG1 is a discrete part whose reference comes from its own
crystal; ADL-N is integrated and its reference is strapped/derived at the platform level. I could
not source a document that states ADL-N's reference directly. **Do not assume.** Read `SKL_DSSM`
and log it — see §13.

#### CDCLK ratios for ADL-N

`[I915]` uses `adlp_cdclk_table` and `tgl_cdclk_funcs` for ADL-N
(`display/intel_cdclk.c:3745-3772`; ADL-N is `IS_ALDERLAKE_P` and is not A0/B0 stepping, so it
takes the final `else` branch).

| ref = 19.2 MHz | ratio | ref = 24 MHz | ratio | ref = 38.4 MHz | ratio |
|---|---|---|---|---|---|
| 172.800 MHz | 27 | 176.000 MHz | 22 | 179.200 MHz | 14 |
| 192.000 MHz | 20 | 192.000 MHz | 16 | 192.000 MHz | 10 |
| 307.200 MHz | 32 | 312.000 MHz | 26 | 307.200 MHz | 16 |
| 556.800 MHz | 58 | 552.000 MHz | 46 | 556.800 MHz | 29 |
| 652.800 MHz | 68 | 648.000 MHz | 54 | 652.800 MHz | 34 |

`[I915]` `display/intel_cdclk.c:1354-1373`.

The PLL VCO is `vco = ratio * ref`. The CD2X divider is then `vco / cdclk`, which must be one of
`2, 3, 4, 8` (encoded `CD2X_DIV_SEL = 1, 1.5, 2, 4` respectively, i.e. the *divider value* is
`2/1.5/2/4`… note the encoding is `DIV_SEL_1`, `DIV_SEL_1_5`, `DIV_SEL_2`, `DIV_SEL_4` for
`cdclk = vco/2/div` with `div ∈ {1, 1.5, 2, 4}`). `[I915]`
`display/intel_cdclk.c:1801-1825` — the comment there states the relation exactly:
`cdclk = vco / 2 / div{1,1.5,2,4}`.

Example: ref 19.2 MHz, cdclk 307.2 MHz → ratio 32 → vco 614.4 MHz → `614.4/2/1 = 307.2` → `div = 1`
→ `BXT_CDCLK_CD2X_DIV_SEL_1`.

#### Changing CDCLK

`[I915]` `bxt_set_cdclk` (`display/intel_cdclk.c:2069-2135`) wraps `_bxt_set_cdclk`
(`intel_cdclk.c:2035-2067`). Sequence:

1. **Tell the power controller first.** Gen11–Gen13 use the PCode mailbox:
   `skl_pcode_request(SKL_PCODE_CDCLK_CONTROL /*0x7*/, SKL_CDCLK_PREPARE_FOR_CHANGE /*0x3*/,
   SKL_CDCLK_READY_FOR_CHANGE /*0x1*/, SKL_CDCLK_READY_FOR_CHANGE, 3 retries)`.
   `[I915]` `display/intel_cdclk.c:2081-2088`; constants at `i915_reg.h:3512-3514`.
2. **Program the PLL.** ADL-N has `HAS_CDCLK_CRAWL`, so if the PLL is already running and both old
   and new VCO are known, i915 uses a *crawl* — retune without disabling:
   ```
   write(CDCLK_PLL_ENABLE, RATIO(new) | PLL_ENABLE)
   write(CDCLK_PLL_ENABLE, RATIO(new) | PLL_ENABLE | FREQ_REQ)
   poll(CDCLK_PLL_ENABLE, LOCK | FREQ_REQ_ACK)      /* timeout 200us */
   write(CDCLK_PLL_ENABLE, RATIO(new) | PLL_ENABLE) /* clear FREQ_REQ */
   ```
   Otherwise (PLL off, or VCO unknown) i915 falls back to disable/enable:
   ```
   write(CDCLK_PLL_ENABLE, RATIO(new))
   write(CDCLK_PLL_ENABLE, RATIO(new) | PLL_ENABLE)
   poll(CDCLK_PLL_ENABLE, LOCK)                      /* timeout 200us */
   ```
   `[I915]` `display/intel_cdclk.c:1768-1790` (`adlp_cdclk_pll_crawl`), `1750-1766`
   (`icl_cdclk_pll_enable`). `[PRM]` gives the same enable step and the same 200 µs lock timeout.
3. **Program `CDCLK_CTL`**: `CD2X_DIV_SEL | CD2X_PIPE(pipe) | CDCLK_FREQ_DECIMAL(cdclk)`.
   The decimal field is computed by `[I915]` as
   `skl_cdclk_decimal(cdclk) = DIV_ROUND_CLOSEST(cdclk - 1000, 500)` (`intel_cdclk.c:1024-1027`)
   — i.e. `(cdclk_in_kHz - 1000) / 500`, rounded.
4. **If a pipe is active, wait for its next vblank** before the change takes effect.
   `[I915]` `_bxt_set_cdclk` ends with `intel_crtc_wait_for_next_vblank()`.
5. **Tell the power controller the voltage level**: `snb_pcode_write(SKL_PCODE_CDCLK_CONTROL,
   voltage_level)`.

`[PRM]`'s "Sequence for Changing CD Clock Frequency" matches this and adds the important rule:
**unless you are changing only the CD2X divider, disable all display engine functions first.**
Power wells may stay enabled.

`[GAP]` I did not source the voltage-level table for ADL-N. `[I915]` uses `tgl_calc_voltage_level`
(`intel_cdclk.c:1557-1569`), which maps CDCLK to a discrete voltage index; the values are specific
to Gen12 and I did not verify them against a PRM. If you are not changing CDCLK dynamically —
and for a first light-up you are not; you pick one value and stay there — you can program the
voltage level once from that table, or, more conservatively, drive the highest voltage level.

#### The PCode mailbox protocol

Needed for CDCLK and for the watermark latency read (§7.4). `[I915]` `intel_pcode.c`
`__snb_pcode_rw`:

```
poll GEN6_PCODE_MAILBOX (0x138124) bit GEN6_PCODE_READY(31) == 0   /* else -EAGAIN */
write GEN6_PCODE_DATA  (0x138128) = val
write GEN6_PCODE_DATA1 (0x13812C) = val1 (0 if unused)
write GEN6_PCODE_MAILBOX = GEN6_PCODE_READY | command
poll GEN6_PCODE_MAILBOX bit READY == 0                              /* timeout */
if read: read GEN6_PCODE_DATA (and DATA1)
check error code in GEN6_PCODE_MAILBOX[7:0] == GEN6_PCODE_SUCCESS (0)
```

Mailbox bitfields: `GEN6_PCODE_MB_PARAM2[23:16]`, `GEN6_PCODE_MB_PARAM1[15:8]`,
`GEN6_PCODE_MB_COMMAND[7:0]`. `[I915]` `i915_reg.h:3484-3510`.

`[PRM]` describes the same mailbox as the "GT Driver Mailbox" with `Run/Busy`, `Address Control`,
`Command/Error Code` fields and a **100 µs** timeout for the watermark commands.

### 4.7 DBUF

The DBUF (display buffer) is a small SRAM that absorbs display memory-read latency. On `XE_LPD` it
is **4096 blocks** across **4 slices** `DBUF_S1..DBUF_S4`, block size **512 bytes**.
`[I915]` `intel_display_device.c:1022-1023` (`.dbuf.size = 4096`, `slice_mask = S1|S2|S3|S4`);
block size `[PRM]` "Watermark Algorithm" (the worked example divides bytes by 512).

| Register | Offset |
|---|---|
| `DBUF_CTL_S0` | `0x45008` |
| `DBUF_CTL_S1` | `0x44FE8` |
| `DBUF_CTL_S2` | `0x44300` |
| `DBUF_CTL_S3` | `0x44304` |

`[I915]` `skl_watermark_regs.h:54-62`. Note the **non-monotonic addresses** and the comment there:
on Gen12 the slices are numbered `S1..S4` and from display 13 the hardware team renumbers them
`S0..S3` **without changing the addresses**. So `DBUF_CTL_S1 (0x44FE8)` is old "slice 1", and
i915's index 0 (`0x45008`) is old slice… this is genuinely confusing and the source comment says so.
`[INF]` For a first bring-up, sidestep it: enable **all** slices and give pipe A the whole buffer.

Per-slice fields: `DBUF_POWER_REQUEST[31]`, `DBUF_POWER_STATE[30]`,
`DBUF_TRACKER_STATE_SERVICE[23:19]`, and for **ADL-P+** `DBUF_MIN_TRACKER_STATE_SERVICE[18:16]`.
`[I915]` `skl_watermark_regs.h:63-68`.

Enable handshake (identical shape to a power well):
```
set DBUF_POWER_REQUEST
poll DBUF_POWER_STATE == 1        /* [PRM] timeout 10us */
```
`[PRM]` "Initialize Sequence" step 5; `[I915]` `gen9_dbuf_slices_update`.

`[I915]` `gen12_dbuf_slices_config` sets `DBUF_TRACKER_STATE_SERVICE = 8` on every slice — but
**it returns early for ADL-P**, so ADL-N does not get this programming from i915.
`[I915]` `display/intel_display_power.c` `gen12_dbuf_slices_config`. `[INF]` This is most likely
because the ADL-P default is already correct, or because a different value is programmed elsewhere.
`[GAP]` I could not find where ADL-P programs `DBUF_TRACKER_STATE_SERVICE`; treat "leave it at its
reset value" as the starting position and change it only if you observe underruns.

For a first light-up, `[I915]`'s own strategy is the one to copy: *"Just power up at least 1 slice,
we will figure out later which slices we have and what we need."* Enable the slices reported as
present, and allocate all of them to the one active pipe.

### 4.8 Raw clock

The "south display" (GMBUS, hotplug, panel power, backlight) runs off a raw clock derived from a
crystal. Its frequency must be programmed into `PCH_RAWCLK_FREQ` (`0xC6204`) **before** any south
display function is enabled. `[PRM]` "General", South Display Engine: *"If the actual frequency is
something else, then RAWCLK_FREQ must be configured before enabling south display functions."*

| Field | Bits |
|---|---|
| `RAWCLK_FREQ_MASK` | `[9:0]` |
| `ICP_RAWCLK_NUM(x)` | `x << 11` |
| `CNP_RAWCLK_DIV(x)` | `x << 16` |
| `CNP_RAWCLK_DEN(x)` | `x << 26` |

`[I915]` `i915_reg.h:3133-3143`.

`[I915]` `cnp_rawclk` (`display/intel_cdclk.c:3560-3580`) implements the whole thing for the
platforms in the `PCH_CNP`…`PCH_DG1` range, which is where ADL-N sits:

```
if (SFUSE_STRAP & SFUSE_STRAP_RAW_FREQUENCY):   /* bit 8 */
    divider = 24000 ; fraction = 0        -> 24.0 MHz
else:
    divider = 19000 ; fraction = 200      -> 19.2 MHz
rawclk = CNP_RAWCLK_DIV(divider/1000)
if fraction:
    rawclk |= CNP_RAWCLK_DEN(round(1000/fraction) - 1)
    rawclk |= ICP_RAWCLK_NUM(1)
write(PCH_RAWCLK_FREQ, rawclk)
return divider + fraction                 /* kHz: 24000 or 19200 */
```

So concretely:
- 24 MHz → `PCH_RAWCLK_FREQ = 24 << 16 = 0x00180000`
- 19.2 MHz → `(19 << 16) | (4 << 26) | (1 << 11) = 0x00130000 | 0x10000000 | 0x800 = 0x10130800`

**This is again a source disagreement.** `[PRM]` (DG1) says *"Raw clock frequency is expected to be
38.4 MHz. The RAWCLK_FREQ register … defaults to 38.4 MHz"* and DG1's `dg1_rawclk` programs
`DEN(4) | DIV(37) | NUM(2)` for 38.4 MHz. `[I915]`'s `cnp_rawclk`, used for the integrated
platforms, offers only 24 MHz or 19.2 MHz. **Which to trust:** on ADL-N guess nothing — read
`SFUSE_STRAP` bit 8 and program accordingly, then sanity-check by measuring a known timing
(§13). If the raw clock is wrong, GMBUS arbitration timing and hotplug de-glitching are wrong, which
presents as *intermittent* EDID failures rather than clean ones.

### 4.9 Initialisation and un-initialisation

`[PRM]` "Initialize Sequence", and `[I915]` `icl_display_core_init`
(`display/intel_display_power.c`) implement the same list. i915's version is the one to follow
because it is Gen13-correct:

```
0. gen9_set_dc_state(DC_STATE_DISABLE)                  /* DC_STATE_EN = 0x45504 */
1. PCH reset handshake (NDE_RSTWRN_OPT)
2. intel_combo_phy_init()                               /* §9.3 */
3. enable PW_1  (with the DISABLE_FLR_SRC workaround)
4. intel_cdclk_init_hw()                                /* §4.6 */
   gen12_dbuf_slices_config()                           /* no-op on ADL-P/N */
5. gen9_dbuf_enable()                                   /* §4.7 */
6. icl_mbus_init()
7. tgl_bw_buddy_init()                                  /* BW_BUDDY_CTL 0x45130/0x45140 */
8. (DG2 only) PHY calibration
9. (resume only) intel_dmc_load_program()
10. Wa_14011508470: GEN11_CHICKEN_DCPR_2 (0x46434)
11. Wa_14011503030 (xelpd): XELPD_DISPLAY_ERR_FATAL_MASK (0x4421c) = ~0
```

`[I915]` `display/intel_display_power.c`, `icl_display_core_init`. Register addresses from
`i915_reg.h:2485,2819-2826,2849-2853,4364-4373`.

Step 11 is worth calling out: **`XELPD_DISPLAY_ERR_FATAL_MASK` is set to all-ones**, i.e. all fatal
display errors masked. On a first bring-up you may prefer the opposite — leave them unmasked so you
can see what breaks. `[INF]`: i915 masks them because they are noisy; you want the noise.

`[PRM]` "Un-initialize Sequence" — **only run this as part of DC9**. It conflicts with other DC
states. The order is the exact reverse: disable all pipes/transcoders/ports/planes/wells above PG1,
then DBUFs, then CDCLK, then PG1, then un-init combo PHYs. For a first bring-up you will never run
this, but it is what a clean shutdown looks like.

### 4.10 DMC — the honest answer

The DMC (Display Micro-Controller) is a microcontroller that owns power-well and DC-state
transitions in hardware. `has_dmc = 1` for `xe_lpd_display`. `[I915]` `intel_display_device.c:1074`.

**You do not need the DMC firmware for a first modeset.** The DMC matters when the display engine is
allowed to enter DC5/DC6 (deep power states); if you keep `DC_STATE_EN = 0` (DC disabled) — which
`icl_display_core_init` does at step 0 — the hardware never asks the DMC to do anything, and the
power wells stay under your direct control.

`[INF]` This is the single biggest scope reduction in the power area, and it is safe for a first
light-up. It is also what the PRM implies: the DMC is only mentioned in the context of DC states.

`[GAP]` I did not source whether a DMC-less ADL-N has any *other* required firmware. i915 loads DMC
firmware from the filesystem; on a from-scratch kernel with no firmware, keeping DC disabled avoids
the question entirely.

---

## 5. Pipes, transcoders, planes — topology

### 5.1 The shape of the pipeline

`[PRM]` "DG1 Display Overview": *"The front end of the display contains the pipes. The pipes connect
to the transcoders. The transcoders … connect to the DDIs to drive the IO/PHY."*

```
  plane 1 (primary) ─┐
  plane 2..5 (sprite)├─► pipe blender ──► pipe scaler ──► TRANSCONF ──► TRANS_DDI_FUNC_CTL ──► DDI ──► PHY
  cursor ────────────┘        (color, dither)              (timing)      (mode select)
```

Key topology rules, all from `[PRM]` "DG1 Display Overview":

- **Four pipes, A–D.** `[I915]` `xe_lpd_display` sets `pipe_mask = A|B|C|D`.
- **Transcoders A–D are tied 1:1 to pipes A–D**; "Each pipe output can go to either the respective
  transcoder or to transcoders WD\*". `[I915]` encodes the same 1:1 mapping in the `enum pipe` /
  `enum transcoder` values (`intel_display_limits.h:14-50`, with the explicit comment *"the code
  assumes that TRANSCODER_A=0"*).
- **Any transcoder can connect to any DDI.** `[PRM]`: *"Transcoders A-D can connect to any DDI."*
  This is why there are two separate selection registers: `TRANS_CLK_SEL` (transcoder → port clock)
  and `TRANS_DDI_FUNC_CTL`'s port-select field (transcoder → DDI), plus `DPCLKA_CFGCR0`
  (DDI → PLL). Three independent muxes.
- **A pipe cannot drive more than one display, and cannot connect to more than one transcoder
  simultaneously.** (Twin modes are not supported on Xe-LP.)
- **ADR-N has no wireless transcoders and no DSI** unless the SKU wires up a panel. `[I915]`
  `xe_lpd_display` declares `TRANSCODER_DSI_0/1` in `cpu_transcoder_mask`, but that is the
  maximum. `[PRM]` lists WD/DSI transcoders for DG1; they are irrelevant to a desktop ADL-N.

### 5.2 Pipe registers

| Register | Pipe A | Field summary |
|---|---|---|
| `TRANSCONF` (a.k.a. `PIPECONF`; **i915 renamed it**) | `0x70008` | `ENABLE[31]` (a request), `STATE_ENABLE[30]` (**a status: i915 polls it, never sets it**), `INTERLACE[23:21]` |
| `PIPESRC` | `0x6001c` | `WIDTH[31:16]`, `HEIGHT[15:0]` — pipe source size, **minus 1** |
| `PIPEDSL` | `0x70000` | `LINE[19:0]` — current scanline; read this to prove the pipe is running |
| `PIPESTAT` | `0x70024` | `PIPE_FIFO_UNDERRUN_STATUS[31]`, vblank status/enable |
| `PIPE_MISC` | `0x70030` | `YUV420_*`, `HDR_MODE_PRECISION[23]`, `PIXEL_ROUNDING_TRUNC[8]` |
| `PIPE_ARB_CTL` | `0x70028` | `USE_PROG_SLOTS[13]` |

`[I915]` `i915_reg.h:1584-1720`. Pipe B adds `0x1000`, C adds `0x2000`, D adds `0x3000`.

> **Naming warning.** `[I915]` v6.12 calls `0x70008` **`TRANSCONF`**; the PRM and every older
> source call it **`PIPECONF`**. They are the same register. `[I915]` `i915_reg.h:1588` defines
> `_TRANSACONF 0x70008` and `TRANSCONF(dev, trans)` at `i915_reg.h:1691`. Do not go looking for a
> separate `PIPECONF` register; there isn't one.

### 5.3 Transcoder timing registers

Per transcoder, base `0x60000` (A) / `0x61000` (B) / `0x62000` (C) / `0x63000` (D).
`[I915]` `i915_reg.h:1072-1110`, `intel_display_device.c:73-77`.

| Register | Offset from base | Layout |
|---|---|---|
| `HTOTAL` | `+0x00` | `[31:16]` = total−1, `[15:0]` = active−1 |
| `HBLANK` | `+0x04` | `[31:16]` = end−1, `[15:0]` = start−1 |
| `HSYNC` | `+0x08` | `[31:16]` = end−1, `[15:0]` = start−1 |
| `VTOTAL` | `+0x0C` | `[31:16]` = total−1, `[15:0]` = active−1 |
| `VBLANK` | `+0x10` | `[31:16]` = end−1, `[15:0]` = start−1 |
| `VSYNC` | `+0x14` | `[31:16]` = end−1, `[15:0]` = start−1 |

**All six registers store `value − 1` in both halves.** This is the classic off-by-one that makes a
"nothing on screen" bring-up. `[PRM]` documents them as `HACTIVE`, `HTOTAL` etc. with the same
minus-one convention; `[I915]` encodes it in the field macros `HACTIVE(hdisplay)`, `HTOTAL(htotal)`
etc. where the *caller* passes the already-decremented value.

`TRANS_VSYNCSHIFT` `+0x28`, `BCLRPAT` `+0x20`, `TRANS_MULT` `+0x2c` — leave at reset for a
progressive RGB mode.

### 5.4 Plane registers

Universal planes (Gen9+, still current on Gen12). Plane 1 is the **primary**; planes 2–5 are sprites;
the cursor is separate. `[I915]` `skl_universal_plane_regs.h`.

Per (pipe, plane) — plane 1 at `0x70180` (pipe A), plane 2 at `0x70280`, i.e. `+0x100` per plane,
`+0x1000` per pipe:

| Register | Plane 1 (A) | Layout |
|---|---|---|
| `PLANE_CTL` | `0x70180` | see §5.5 |
| `PLANE_STRIDE` | `0x70188` | `[11:0]` stride in bytes |
| `PLANE_POS` | `0x7018C` | `[31:16]` = Y, `[15:0]` = X (screen position) |
| `PLANE_SIZE` | `0x70190` | `[31:16]` = height−1, `[15:0]` = width−1 |
| `PLANE_SURF` | `0x7019C` | `[31:12]` = GGTT address, `[2]` = decrypt. **Write last.** |
| `PLANE_OFFSET` | `0x701A4` | `[31:16]` = Y, `[15:0]` = X (source offset within surface) |
| `PLANE_SURFLIVE` | `0x701AC` | readback of the live surface address |
| `PLANE_BUF_CFG` | `0x7027C` | DDB allocation — see §7 |
| `PLANE_WM(level)` | `0x70240 + level*4` | watermark — see §7 |
| `PLANE_COLOR_CTL` | `0x701CC` | `ALPHA[5:4]`, `CSC`, `GAMMA` |
| `PLANE_CUS_CTL` | `0x701C8` | chroma upsampler (HDR planes only) |

`[I915]` `skl_universal_plane_regs.h:31-380`.

### 5.5 `PLANE_CTL` — the fields a linear RGB scanout needs

`[I915]` `skl_universal_plane_regs.h:35-100`:

| Field | Bits | Value for linear XRGB8888 |
|---|---|---|
| `PLANE_CTL_ENABLE` | 31 | `1` |
| `PLANE_CTL_FORMAT_MASK_ICL` | `[27:23]` | `PLANE_CTL_FORMAT_XRGB_8888` = `4 << 24` |
| `PLANE_CTL_ORDER_RGBX` | 20 | `0` for XRGB/BGRX layout, `1` for XBGR |
| `PLANE_CTL_TILED_MASK` | `[12:10]` | `PLANE_CTL_TILED_LINEAR` = `0` |
| `PLANE_CTL_ALPHA_MASK` | `[5:4]` | `PLANE_CTL_ALPHA_DISABLE` = `0` |
| `PLANE_CTL_ROTATE_MASK` | `[1:0]` | `PLANE_CTL_ROTATE_0` = `0` |
| `PLANE_CTL_TRICKLE_FEED_DISABLE` | 14 | leave `0` unless debugging |

Note the Gen12 format field is `[27:23]` (5 bits) whereas Skylake's was `[27:24]` (4 bits). The
comment in the source says the shift-24 values still map correctly as long as bit 23 stays 0, which
is why `PLANE_CTL_FORMAT_XRGB_8888` is defined with the SKL mask. `[I915]`
`skl_universal_plane_regs.h:43-49`.

`skl_plane_ctl_format` maps DRM fourcc to these bits (`[I915]`
`skl_universal_plane.c`, `skl_plane_ctl_format`), and `skl_plane_ctl_tiling` maps modifiers
(`skl_plane_ctl_tiling`). For the simplest bring-up use `DRM_FORMAT_XRGB8888` +
`DRM_FORMAT_MOD_LINEAR`, which is `PLANE_CTL_ENABLE | (4<<24)`.

### 5.6 The plane programming order — and why it matters

`[I915]` splits plane commit into a `noarm` half and an `arm` half
(`display/skl_universal_plane.c`, `skl_plane_update_noarm` / `skl_plane_update_arm`). The order is
deliberate:

**`noarm` (safe to do early, does not take effect until the surface register is written):**
```
PLANE_STRIDE   <- stride
PLANE_POS      <- y | x
PLANE_SIZE     <- (h-1) | (w-1)
PLANE_WM(0..n) <- watermarks, PLANE_WM_TRANS
PLANE_BUF_CFG  <- DDB start/end
```

**`arm` (takes effect at the next flip):**
```
PLANE_KEYVAL, PLANE_KEYMSK, PLANE_KEYMAX   <- 0 for no colour key
PLANE_OFFSET   <- source x/y within the surface
PLANE_AUX_DIST, PLANE_AUX_OFFSET           <- 0 for single-plane formats
PLANE_COLOR_CTL                            <- alpha disable, no CSC
(scaler, if any: enable BEFORE the plane)
PLANE_CTL      <- enable + format + tiling
PLANE_SURF     <- GGTT address        <-- THIS ARMS EVERYTHING
```

The source comment states the reason for the last two writes being adjacent and in that order:
*"The control register self-arms if the plane was previously disabled. Try to make the plane enable
atomic by writing the control register just before the surface register."*
`[I915]` `skl_universal_plane.c`, `skl_plane_update_arm`.

`[PRM]` says the same thing independently: *"Write the plane surface base address register to
trigger update of the watermarks and other plane double buffered registers. This should be done only
after all plane configuration is configured to match the new watermark values."* `[PRM]+[I915]`
agree — **`PLANE_SURF` is the commit.**

**Disable** is the mirror image and is two writes:
```
PLANE_CTL  <- 0
PLANE_SURF <- 0
```
`[I915]` `skl_plane_disable_arm`. (The ICL variant additionally clears `PLANE_CUS_CTL` for HDR
planes and `SEL_FETCH_PLANE_CTL` if PSR2 selective fetch was on — neither applies to a first
light-up.)

---

## 6. Timings

### 6.1 From a mode to registers

Given a `drm_mode`-style timing with `hdisplay`, `hsync_start`, `hsync_end`, `htotal`, `vdisplay`,
`vsync_start`, `vsync_end`, `vtotal` and polarity flags, program transcoder T:

```
HTOTAL(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)
HBLANK(T)  = ((htotal  - 1) << 16) | (hdisplay - 1)     /* blanking runs to end of line */
HSYNC(T)   = ((hsync_end - 1) << 16) | (hsync_start - 1)
VTOTAL(T)  = ((vtotal  - 1) << 16) | (vdisplay - 1)
VBLANK(T)  = ((vtotal  - 1) << 16) | (vdisplay - 1)
VSYNC(T)   = ((vsync_end - 1) << 16) | (vsync_start - 1)
PIPESRC(T) = ((hdisplay - 1) << 16) | (vdisplay - 1)
```

`[I915]`'s field macros take the already-decremented value for **every** one of these fields
(`i915_reg.h:1072-1110`), and its `PIPESRC` macro likewise (`PIPESRC_WIDTH(w)`, `PIPESRC_HEIGHT(h)`).
`[PRM]` names the fields `HACTIVE`/`HTOTAL`/… without stating the encoding in the text I extracted;
`[INF]` the minus-one convention is confirmed by the i915 field names and by the fact that
`PLANE_SIZE` uses the same convention.

Polarity does **not** go here — it goes in `TRANS_DDI_FUNC_CTL` as `TRANS_DDI_PHSYNC` /
`TRANS_DDI_PVSYNC`. `[I915]` `display/intel_ddi.c:471-580`.

### 6.2 Pixel clock and CDCLK

The pixel clock comes from the port PLL, through the DDI, through `TRANS_CLK_SEL`, into the
transcoder. CDCLK only has to be **fast enough**: the PRM requires CDCLK to bound the maximum
pixel rate. `[INF]` from `[PRM]` "Restrictions": for a single 1080p60 mode (~148.5 MHz pixel clock)
the lowest ADL-N CDCLK in the table (172.8 MHz) is ample; pick the lowest and avoid a CDCLK change.

### 6.3 The PLL

#### Structure on ADL-N

`[I915]` `adlp_plls[]` (`display/intel_dpll_mgr.c:4273-4283`) is the authority for ADL-N:

| PLL | id | Type | Registers | Used for |
|---|---|---|---|---|
| DPLL 0 | 0 | `combo_pll_funcs` | `DPLL0_*` | Combo PHY A — **this is the one you want** |
| DPLL 1 | 1 | `combo_pll_funcs` | `DPLL1_*` | Combo PHY B |
| TBT PLL | 2 | `tbt_pll_funcs` | `0x46020` | Type-C TBT (genlock filtering) |
| TC PLL 1–4 | 3–6 | `dkl_pll_funcs` | `PORTTC1/2_PLL_ENABLE` `0x46038`/`0x46040` | Type-C ports |

For an HDMI/DVI monitor on the rear port — which on ADL-N is **combo PHY A or B** `[GFXINIT]`
(independent confirmation: ADL-N's rear HDMI is on a combo PHY, DP++ over TC1) — **DPLL0 or DPLL1
is all you need.** Ignore the DKL/Type-C PLLs entirely.

#### Registers

| Register | Offset | Purpose |
|---|---|---|
| `DPLL0_CFGCR0` | `0x164284` | DCO integer + fraction |
| `DPLL0_CFGCR1` | `0x164288` | pdiv/kdiv/qdiv/central freq |
| `DPLL1_CFGCR0` | `0x16428C` | the same fields as `DPLL0_CFGCR0`; this is combo PHY B's pair |
| `DPLL1_CFGCR1` | `0x164290` | the same fields as `DPLL0_CFGCR1` |
| `DPLL0_DIV0` | `0x164B00` | AFC startup (only if VBT overrides it) |
| `DPLL1_DIV0` | `0x164C00` | the same, for DPLL1 |
| `DPLL0_ENABLE` (**= `LCPLL1_CTL`**) | `0x46010` | `PLL_ENABLE[31]`, `LOCK[30]`, `POWER_ENABLE[27]`, `POWER_STATE[26]` |
| `DPLL1_ENABLE` (**= `LCPLL2_CTL`**) | `0x46014` | same bits |
| `ICL_DPCLKA_CFGCR0` | `0x164280` | DDI → PLL select + per-DDI clock-off |

`[I915]` `i915_reg.h:4301-4322`, `4213-4221`, `4158`.

> **The `DPLL1_CFGCR*` rows, and how this table came to be missing them.** Earlier revisions stated
> the config offsets for DPLL0 only, as "`DPLLn_CFGCR0` (`0x164284` for DPLL0)" and "`DPLLn_CFGCR1`
> (`0x164288` for DPLL0)", which read on its own leaves combo PHY B's PLL with no address: a driver
> built from this table refuses DDI B rather than lighting the screen. The citation printed for the
> whole block, `i915_reg.h:4301-4322`, is precisely the region that defines both pairs --
> `_TGL_DPLL0_CFGCR0/1 = 0x164284/0x164288` at `:4301`/`:4316` and
> `_TGL_DPLL1_CFGCR0/1 = 0x16428C/0x164290` at `:4302`/`:4317`, selected by PLL id through
> `TGL_DPLL_CFGCR0/1(pll)` (`:4304-4306`, `:4319-4321`), which is what `icl_dpll_write` uses for
> `DISPLAY_VER >= 12` (`[I915]` `display/intel_dpll_mgr.c:3767-3769`). The two pairs interleave, so
> DPLL1 is **not** DPLL0 + 4. `DPLL1_DIV0` is listed for symmetry and is written only when the VBT
> overrides the AFC startup value, the same rule as DPLL0's
> (`[I915]` `intel_dpll_mgr.c:3784-3789`); nothing in this kernel writes either.

> **Two names, one register.** `_DPLL0_ENABLE = 0x46010` and `LCPLL1_CTL = 0x46010` are the same
> address; likewise `0x46014`. `[I915]` `i915_reg.h:4093-4095` and `4213-4214`. The `LCPLL_PLL_ENABLE`
> bit (31) is the same as `PLL_ENABLE`. Do not look for a separate register.

#### The divider arithmetic (combo PHY / WRPLL)

This is the part that must be right. The governing identity is the same on every generation:

```
DCO = P · Q · K · afe_clock          where afe_clock = 5 · symbol_rate = 5 · (bit_rate / 2)
```

so `symbol_rate = DCO / (P · Q · K · 5)`. Only the *search* and the *field encoding* differ between
generations, and **ADL-N uses the ICL/TGL forms, not the Skylake ones.** The full ADL-N search is
transcribed in "The search — now resolved from the implementation" below; the essential arithmetic
for converting a chosen DCO into register bits is:

```
dco_integer  = DCO_Hz / (ref_kHz * 1000)
dco_fraction = ((DCO_Hz / ref_MHz) - dco_integer * 1e6) * 0x8000 / 1e6
```

equivalently, and closer to how `icl_wrpll_params_populate` computes it,
`dco = (dco_freq << 15) / ref_freq; dco_integer = dco >> 15; dco_fraction = dco & 0x7fff`.

**Two corrections to earlier drafts of this subsection**, both instances of the same mistake —
reaching for the Skylake helper when the target is Gen12:

1. The search description named `skl_wrpll_params_populate` / `skl_ddi_calculate_wrpll` as the
   ADL-N arithmetic. They are the **Skylake** arithmetic. ADL-N uses `icl_calc_wrpll` +
   `icl_wrpll_get_multipliers` + `icl_wrpll_params_populate`.
2. The candidate set was given as `p0 ∈ {1,2,3,7}`, `p2 ∈ {5,2,3,1}` with central frequencies
   `{8.4, 9.0, 9.6} GHz`. The ADL-N path uses a flat **total-divider** list
   `{2,4,…,102, 3,5,7,9,15,21}`, a single **midpoint** of 8999 MHz, and decomposes to
   `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`. See below.

Field encodings. **These are the TGL/Gen12 positions, which are NOT the Skylake positions.**
`[TGL2C]` (DPLL0–3 `CFGCR0`/`CFGCR1`) and `[I915]` `i915_reg.h:4265-4298` (`_ICL_DPLL0_CFGCR1`
block, used for `DISPLAY_VER >= 12`) **agree exactly**:

| Register | Field | Bits | Notes |
|---|---|---|---|
| `DPLLn_CFGCR0` (`0x164284` for DPLL0, `0x16428C` for DPLL1) | `DCO_FRACTION` | `[24:10]` | reset default `0x4000` |
| | `DCO_INTEGER` | `[9:0]` | reset default `0x151` |
| | `LINK_RATE` override | mask `[28:25]`, defined values in `[27:25]` | HDMI link-rate override; leave 0 |
| `DPLLn_CFGCR1` (`0x164288` for DPLL0, `0x164290` for DPLL1) | `QDIV_RATIO` | `[17:10]` | |
| | `QDIV_MODE` | `[9]` | 0 if `qdiv_ratio == 1`, else 1 |
| | `KDIV` | `[8:6]` | `K=1→1`, `K=2→2`, `K=3→4` |
| | `PDIV` | `[5:2]` | `P=2→1`, `P=3→2`, `P=5→4`, `P=7→8` |
| | `CFSELOVRD` | `[1:0]` | **On Gen12 this is `TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL = 0`**, not a central-frequency selector |

The last row is a Gen12 delta that is easy to miss: `icl_calc_dpll_state` writes
`TGL_DPLL_CFGCR1_CFSELOVRD_NORMAL_XTAL` for `DISPLAY_VER >= 12` and only falls back to the ICL
`DPLL_CFGCR1_CENTRAL_FREQ_8400` (`3 << 0`) for Gen11. `[I915]`
`display/intel_dpll_mgr.c` `icl_calc_dpll_state`. On ADL-N, **bits `[1:0]` of `CFGCR1` are 0.**

The earlier SKL-era `DPLL_CFGCR2_*` definitions at `[I915]` `i915_reg.h:4136-4152`
(`QDIV_RATIO[15:8]`, `QDIV_MODE[7]`, `KDIV[6:5]`, `PDIV[4:2]`) are **two bits lower** and do **not**
apply. An earlier draft of this document used them; that was wrong.

#### The PDIV/KDIV encoding — resolved, and a real trap alongside it

**On the ADL-N path there is no discrepancy: write and read agree.** This corrects an earlier draft
of this document which claimed an unresolved conflict. The draft — and, independently, a later
implementation review — both made the same mistake, which is the trap described below.

The chain on ADL-N (HDMI or DSI on a combo PHY) is:

```
icl_calc_wrpll()            -> picks a total divider from its own list, then
icl_wrpll_get_multipliers() -> logical (pdiv, qdiv, kdiv)
icl_wrpll_params_populate() -> encodes into Gen12 CFGCR1 code values
icl_calc_dpll_state()       -> DPLL_CFGCR1_PDIV(pdiv) | DPLL_CFGCR1_KDIV(kdiv)
icl_dpll_write()            -> DPLLn_CFGCR1
```

and `icl_wrpll_params_populate` (`[I915]` `display/intel_dpll_mgr.c:2546-2570`, identical in
mainline at `:2576`) emits **exactly the values the Gen12 named constants define**:

| Logical | `params->kdiv` | Gen12 constant | Field value |
|---|---|---|---|
| `K = 1` | `1` | `DPLL_CFGCR1_KDIV_1` | `1 << 6` |
| `K = 2` | `2` | `DPLL_CFGCR1_KDIV_2` | `2 << 6` |
| `K = 3` | `4` | `DPLL_CFGCR1_KDIV_3` | `4 << 6` |

| Logical | `params->pdiv` | Gen12 constant | Field value |
|---|---|---|---|
| `P = 2` | `1` | `DPLL_CFGCR1_PDIV_2` | `1 << 2` |
| `P = 3` | `2` | `DPLL_CFGCR1_PDIV_3` | `2 << 2` |
| `P = 5` | `4` | `DPLL_CFGCR1_PDIV_5` | `4 << 2` |
| `P = 7` | `8` | `DPLL_CFGCR1_PDIV_7` | `8 << 2` |

The read path `icl_ddi_combo_pll_get_freq` (`[I915]` `display/intel_dpll_mgr.c:2827-2890`) masks
`CFGCR1` with `DPLL_CFGCR1_PDIV_MASK` / `KDIV_MASK` and switches on **those same named constants**.
Write and read therefore round-trip. The hard-coded DP tables
(`icl_dp_combo_pll_{24,19_2}MHz_values`) are pre-shifted the same way — the source comment says
*"These values already adjusted: they're the bits we write to the registers, not the logical
values"*, and `.pdiv = 0x4 /* 5 */` decodes as `P = 5`. Verified numerically: the 19.2 MHz table's
540 MHz entry is `dco_integer = 0x1A5` (421) with `dco_fraction = 0x7000`, giving
`DCO = 421 × 19.2 + (28672 × 19.2)/32768 = 8100 MHz`, and `8100 / (P·Q·K·5) = 8100/(3·1·1·5) = 540`
— correct for `P = 3, Q = 1, K = 1`.

##### The trap: two populate functions, one struct, two encodings

`[I915]` contains **two** functions whose names differ by four characters and which both write the
**same** `struct skl_wrpll_params` — but whose `pdiv` and `kdiv` fields hold **different encodings**:

| | `skl_wrpll_params_populate` | `icl_wrpll_params_populate` |
|---|---|---|
| Source | `intel_dpll_mgr.c:1592` | `intel_dpll_mgr.c:2546` |
| Written to | `DPLL_CFGCR2_*` (Skylake) | `DPLL_CFGCR1_*` (Gen12) |
| Called by | `skl_ddi_calculate_wrpll` → `skl_ddi_hdmi_pll_dividers` | `icl_calc_wrpll` / `icl_calc_dp_combo_pll` / `icl_calc_tbt_pll` |
| Used on | SKL/KBL/CFL/CML HDMI | **ICL/TGL/RKL/ADL** |
| `pdiv` accepts | `P ∈ {1,2,3,7}` → `{0,1,2,4}` | `P ∈ {2,3,5,7}` → `{1,2,4,8}` |
| `kdiv` accepts | `K ∈ {5,2,3,1}` → `{0,1,2,3}` | `K ∈ {1,2,3}` → `{1,2,4}` |

The representability asymmetry is the sharp edge:

- **`P = 5` is representable in the Gen12 convention but NOT in the Skylake one.**
  `skl_wrpll_get_multipliers` *does* produce `p0 = 5` (for total divider 5:
  `else if (p == 5 || p == 7) { *p0 = p; ... }`), and `skl_wrpll_params_populate`'s `pdiv` switch has
  no case for 5 — it hits `default: WARN(1, "Incorrect PDiv")` and leaves `pdiv` at its
  zero-initialised value, which is `PDIV_1`, i.e. `P = 1`. **Silently wrong, not merely warned.**
- **`K = 5` is representable in the Skylake convention but NOT in the Gen12 one.**
  `icl_wrpll_params_populate`'s `kdiv` switch has cases 1, 2, 3 only.

`icl_calc_dpll_state` consumes `pll_params->pdiv`/`kdiv` **assuming the ICL convention**. Nothing in
the code prevents a Skylake-encoded `struct skl_wrpll_params` from being handed to it; the compiler
cannot tell the two apart because they are the same type. So this is a latent, silent
mis-programming hazard rather than an active bug — but it is exactly the shape of mistake a
from-scratch implementer makes when copying one populate function and one state-builder from
different generations. **Take the encoder and the decoder from the same generation.**

##### The PRM bounds are matched by the ADL-N path, but not by every list

An implementation review reported that "the PRM's own stated bounds do not cover i915's dividers —
`total = 35` has no PRM-legal `(P,Q,K)`, and `K = 5` is not a legal `K`". That is **half right, and
the half that is right does not apply to ADL-N**:

- `total = 35` → `skl_wrpll_get_multipliers(35)` → `(P,Q,K) = (7,1,5)`. `K = 5` **is** outside the
  PRM's `K ∈ {1,2,3}`, so `total = 35` has no PRM-legal decomposition. **But 35 appears only in
  `skl_wrpll_get_multipliers`'s odd list `{3,5,7,9,15,21,35}`. The ADL-N list in `icl_calc_wrpll` is
  `{2,4,…,102, 3,5,7,9,15,21}` — it stops at 21 and contains no 35.** So this cannot arise on ADL-N.
- Conversely, the ADL-N path **does** respect the PRM bounds. `icl_wrpll_get_multipliers`
  (`intel_dpll_mgr.c:2507-2543`) only ever emits `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`, and sets `Q = 1`
  whenever `K ≠ 2` — which is precisely the PRM's rule, enforced by its own
  `WARN_ON(kdiv != 2 && qdiv != 1)`.

So the document's earlier presentation of the PRM bounds as the ADL-N rule was **correct**; what was
missing is that those bounds are *not* universal to i915, and a reader who wandered into the
Skylake list would find dividers the Gen12 rules forbid.

##### Two more facts the arithmetic depends on

**1. A 38.4 MHz reference is divided down to 19.2 MHz before use.** `[I915]`
`display/intel_dpll_mgr.c:2764-2776`:

```c
static int icl_wrpll_ref_clock(struct drm_i915_private *i915)
{
        int ref_clock = i915->display.dpll.ref_clks.nssc;
        /* For ICL+, the spec states: if reference frequency is 38.4,
         * use 19.2 because the DPLL automatically divides that by 2. */
        if (ref_clock == 38400)
                ref_clock = 19200;
        return ref_clock;
}
```

This is used on **both** sides — `icl_calc_wrpll` (`:2783`) and `icl_ddi_combo_pll_get_freq`
(`:2827`) — so the arithmetic is symmetric. Consequence for a from-scratch driver on a 38.4 MHz
strap: **compute `dco_integer`/`dco_fraction` against 19.2 MHz, not 38.4 MHz**, or every divider
will be wrong by 2×. The DP tables handle this by construction — the 19.2 MHz table carries the
comment *"Also used for 38.4 MHz values"* (`intel_dpll_mgr.c:2637`).

**2. On ADL-P/N at a 38.4 MHz reference, the programmed DCO fraction is halved.** `[I915]`
`display/intel_dpll_mgr.c:2593-2602`:

```c
/* Display WA #22010492432: ehl, tgl, adl-s, adl-p
 * Program half of the nominal DCO divider fraction value. */
static bool ehl_combo_pll_div_frac_wa_needed(...)
{
        return ((IS_ELKHARTLAKE(...) || IS_TIGERLAKE(i915) || IS_ALDERLAKE_S(i915) ||
                 IS_ALDERLAKE_P(i915)) && i915->display.dpll.ref_clks.nssc == 38400);
}
```

`IS_ALDERLAKE_P` is true for ADL-N (it is a subplatform), so **this workaround applies to the
target**. The halving is applied on write in `icl_calc_dpll_state`
(`dco_fraction = DIV_ROUND_CLOSEST(dco_fraction, 2)`, `:2873`) and undone on read in
`icl_ddi_combo_pll_get_freq` (`dco_fraction *= 2`, `:2891`), so it round-trips. Note the predicate
tests `ref_clks.nssc == 38400` — the **raw** strap value, *not* the divided-down value that
`icl_wrpll_ref_clock` returns. Those two facts interact: at a 38.4 MHz strap you divide the
reference by 2 *and* halve the fraction, which is self-consistent because the DPLL itself divides
the reference by 2.

**Do not synthesise these fields by hand from this document.** Two safe routes:

1. **Best:** if the firmware or a vendor driver has already programmed a combo DPLL for a mode you
   can use, **read `DPLL0_CFGCR0`/`DPLL0_CFGCR1` and reuse the divider set verbatim**, changing
   only the ratio if you must. Then diff your own computation against it. This also sidesteps the
   `ehl_combo_pll_div_frac_wa_needed` question, since the stored fraction already has whatever
   adjustment the platform needed.
2. Otherwise mirror `icl_wrpll_get_multipliers` + `icl_wrpll_params_populate` (**not** the `skl_`
   pair), apply `icl_wrpll_ref_clock` and the EHL workaround, and **verify by read-back** before
   enabling.

#### The search — now resolved from the implementation

The PRM states the bounds; i915's implementation of them is `icl_calc_wrpll`
(`[I915]` `display/intel_dpll_mgr.c:2779-2820`, identical in mainline at `:2811`). The earlier draft
of this document recorded the search loop as a `[GAP]`; it is not one. Here it is in full, with the
PRM's stated bounds alongside so the correspondence is visible:

```
afe_clock = port_clock * 5                       /* AFE = 5x symbol rate; port_clock IS the symbol rate */
dco_min = 7998000        /* 7998 MHz  -- PRM: "DCO min 7998"          */
dco_max = 10000000       /* 10000 MHz -- PRM: "DCO max 10000"         */
dco_mid = (dco_min + dco_max) / 2   /* 8999 MHz -- PRM: "midpoint 8999" */

dividers[] = { 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 24, 28, 30, 32, 36, 40,
               42, 44, 48, 50, 52, 54, 56, 60, 64, 66, 68, 70, 72, 76, 78, 80,
               84, 88, 90, 92, 96, 98, 100, 102,
               3, 5, 7, 9, 15, 21 }

best = argmin over d of |afe_clock * dividers[d] - dco_mid|
       subject to  dco_min <= afe_clock * dividers[d] <= dco_max
if no such d: fail with -EINVAL
(P, Q, K) = icl_wrpll_get_multipliers(best)
icl_wrpll_params_populate(params, best_dco, ref_clock, P, Q, K)
```

The three PRM constants and i915's three constants are **the same numbers**, which is a strong
mutual confirmation. The search strategy differs in form only: the PRM describes "closest to the
midpoint" and i915 implements exactly that (`dco_centrality = abs(dco - dco_mid)`, minimised), while
the *Skylake* path (`skl_ddi_calculate_wrpll`) instead walks three discrete central frequencies
`{8400, 9000, 9600}` MHz and minimises percentage deviation with
`SKL_DCO_MAX_PDEVIATION`/`SKL_DCO_MAX_NDEVIATION` bounds. **ADL-N takes the midpoint form, not the
three-frequency form.**

Two behaviours worth knowing because they are easy to get wrong when reimplementing:

- **Even dividers are preferred, but only after the whole search.** In the Skylake loop the even
  list is tried first and the code breaks out if an even divider produced any solution; in the ICL
  loop there is a single flat list with the evens first and strict `<` comparison, so the first
  (smallest) divider achieving the minimum centrality wins.
- **Exhaustive, not early-exit, on the ICL path.** Unlike the Skylake loop there is no
  `min_deviation == 0` early break — every candidate in the list is tested. That is cheap
  (~46 entries) and makes the result order-independent, which is convenient if you want to diff
  your implementation against i915's.

`icl_wrpll_get_multipliers` (`:2507-2543`) then decomposes the chosen total divider:

```
even, == 2          -> P=2, Q=1,      K=1
even, %4 == 0       -> P=2, Q=d/4,    K=2
even, %6 == 0       -> P=3, Q=d/6,    K=2
even, %5 == 0       -> P=5, Q=d/10,   K=2
even, %14 == 0      -> P=7, Q=d/14,   K=2
odd, 3 | 5 | 7      -> P=d, Q=1,      K=1
odd, otherwise (9,15,21) -> P=d/3, Q=1, K=3
```

Note the ordering: `%4` is tested before `%6` before `%5` before `%14`, so e.g. 20 takes the `%4`
branch (`P=2, Q=5, K=2`), not `%5`. **Every branch yields `P ∈ {2,3,5,7}` and `K ∈ {1,2,3}`, and
`Q = 1` whenever `K ≠ 2`** — i.e. the PRM's rules are satisfied by construction, and the
function's own `WARN_ON(kdiv != 2 && qdiv != 1)` is the assertion of that.

**Worked example — 1080p60, 148.5 MHz pixel clock, HDMI, 38.4 MHz strap.** This is worked all the
way through to register values, and every number below was recomputed from the source arithmetic.

```
port_clock = 148500 kHz        (the mode's pixel clock == the TMDS symbol rate)
afe_clock  = 148500 * 5 = 742500 kHz
ref        = 38400 -> icl_wrpll_ref_clock() -> 19200 kHz
```

Candidates `DCO = 742500 * d` (kHz) inside `[7998000, 10000000]`:

| `d` | DCO (MHz) | Verdict |
|---|---|---|
| 10 | 7425.0 | **rejected** — below `dco_min` |
| **12** | **8910.0** | **accepted**, `\|DCO − 8999\| = 89` |
| 14 | 10395.0 | **rejected** — above `dco_max` |

`d = 12` is in fact the *only* in-range candidate here, so the midpoint rule is not exercised by
this mode — it matters at higher pixel clocks where several divisors land in the window.

```
icl_wrpll_get_multipliers(12):  12 % 4 == 0  ->  P = 2, Q = 12/4 = 3, K = 2
icl_wrpll_params_populate(dco_freq = 8910000 kHz, ref_freq = 19200 kHz, P=2, Q=3, K=2):
    dco          = (8910000 << 15) / 19200 = 15206400
    dco_integer  = 15206400 >> 15          = 464   (0x1D0)
    dco_fraction = 15206400 & 0x7FFF       = 2048  (0x800)
```

Because the strap is 38.4 MHz, `ehl_combo_pll_div_frac_wa_needed()` is true and
`icl_calc_dpll_state` halves the fraction before writing:
**`dco_fraction` is written as `1024` (`0x400`), not `2048`.**

Final register values for DPLL0:

| Register | Fields | Value |
|---|---|---|
| `DPLL0_CFGCR0` (`0x164284`) | `DCO_FRACTION[24:10] = 1024`, `DCO_INTEGER[9:0] = 464` | `(1024 << 10) \| 464` = `0x001001D0` |
| `DPLL0_CFGCR1` (`0x164288`) | `QDIV_RATIO[17:10] = 3`, `QDIV_MODE[9] = 1`, `KDIV[8:6] = 2`, `PDIV[5:2] = 1`, `CFSELOVRD[1:0] = 0` | `(3 << 10) \| (1 << 9) \| (2 << 6) \| (1 << 2)` = `0x00000E84` |

Round-trip check (this is what `icl_ddi_combo_pll_get_freq` does, in reverse): the read path doubles
the fraction back to 2048, giving
`DCO = 464 × 19.2 + (2048 × 19.2)/32768 = 8908.8 + 1.2 = 8910.0 MHz`, and then
`symbol_rate = 8910.0 / (P·Q·K·5) = 8910.0 / 60 = 148.5 MHz` ✓ — the pixel clock we started from.

#### PLL enable sequence

`[I915]` `combo_pll_enable` → `icl_pll_power_enable` + `icl_dpll_write` + `icl_pll_enable`
(`display/intel_dpll_mgr.c:1795-1815`, `1740-1793`):

```
1. rmw(DPLL0_ENABLE, 0, PLL_POWER_ENABLE)      /* bit 27 */
   poll(DPLL0_ENABLE, PLL_POWER_STATE)         /* bit 26, timeout 1ms; spec says "immediate" */
2. write(DPLL0_CFGCR0, cfgcr0)
   write(DPLL0_CFGCR1, cfgcr1)
   (optional) rmw(DPLL0_DIV0, AFC_STARTUP_MASK, div0)   /* only if VBT overrides AFC */
   posting_read(DPLL0_CFGCR1)
3. rmw(DPLL0_ENABLE, 0, PLL_ENABLE)            /* bit 31 */
   poll(DPLL0_ENABLE, PLL_LOCK)                /* bit 30; [I915] comment: "Timeout is actually 600us" */
```

Disable is the reverse, with `[I915]`'s comment *"Timeout is actually 1us"* on the unlock poll.

`[PRM]` independently gives the same shape for the CDCLK PLL and notes the PLL output is
`5 × symbol rate`, i.e. the same factor used above. `[PRM]+[I915]` agree on the architecture.

#### DDI→PLL routing

Three separate muxes must agree. `[I915]` `display/intel_ddi.c:1487-1501` and `4158-4172`:

1. **DDI → PLL**: `ICL_DPCLKA_CFGCR0 (0x164280)`, field
   `DDI_CLK_SEL_SHIFT(phy) = phy * 2`, 2 bits, value = **the PLL id** (0 or 1 for DPLL0/DPLL1).
   Also a per-DDI `DDI_CLK_OFF` bit at `_PICK(phy, 10, 11, 24, 4, 5)` (non-RKL encoding).
2. **Transcoder → port clock**: `TRANS_CLK_SEL(tran)` at `0x46140 + tran*4`, field
   `TGL_TRANS_CLK_SEL_PORT(x) = (x + 1) << 28` (Gen12 encoding; the pre-Gen12 encoding is
   `<< 29`). `Disabled` = 0. `[I915]` `i915_reg.h:4007-4015`, `intel_ddi.c:987-1007`.
   **The field is keyed by PHY, not by port, from display version 13 on**: i915 converts with
   `intel_port_to_phy()` (`intel_display.c:1950-1965`) and passes the result to
   `TGL_TRANS_CLK_SEL_PORT` on the `DISPLAY_VER >= 13` arm (`intel_ddi.c:993`, `:999-1000`), where
   version 12 passed `encoder->port`.  For combo PHY A / DDI A the two indices are both 0 and the
   value is the same, so this only bites a port that is not its own PHY (the Type-C DDIs).  Step 3's
   `TRANS_DDI_FUNC_CTL` field is **not** converted -- i915 uses `encoder->port` there on every
   display version (`intel_ddi.c:481`, `:488-490`) -- so the same number is right for one field and
   wrong for the other, which is exactly the kind of thing to write down rather than remember.
3. **Transcoder → DDI**: `TRANS_DDI_FUNC_CTL(tran)`, field
   `TGL_TRANS_DDI_SELECT_PORT(port) = (port + 1) << 27` (Gen12; pre-Gen12 is `<< 28`).
   `[I915]` `i915_reg.h:3742-3760`, `intel_ddi.c:483-490`.

The `+1` in both Gen12 shift values is because port 0 (PORT_A) would otherwise encode as "none".

**Ordering constraint from the spec**, quoted in `[I915]` `_icl_ddi_enable_clock`: the clock-select
write and the clock-off clear *"must be done with separate register writes"*. Two `rmw` calls, not
one.

---

## 7. Planes, DDB and watermarks

### 7.1 The one thing that will stop you

`[PRM]` "Watermark Overview", verbatim in meaning:

> The default settings of the watermark configuration registers **will not allow the display engine
> to operate**. The watermark values must be properly calculated and programmed in order to enable
> a display.

This is not a tuning step. **If you do not program `PLANE_WM` and `PLANE_BUF_CFG`, you get a black
screen or corruption, and no amount of staring at the timing registers will help.**

### 7.2 DDB allocation — the simple version

The DDB (display data buffer) allocation splits the DBUF's 4096 blocks between pipes, and then
between planes within a pipe. Registers: `PLANE_BUF_CFG(pipe, plane)` at `0x7027C` for plane 1 of
pipe A.

Encoding (`[I915]` `skl_universal_plane.c`, `skl_plane_ddb_reg_val`):

```
value = PLANE_BUF_END(end - 1) | PLANE_BUF_START(start)
```
with `PLANE_BUF_START[11:0]`, `PLANE_BUF_END[27:16]`. Field widths are "skl+: 10 bits, icl+ 11 bits,
**adlp+ 12 bits**" `[I915]` `skl_universal_plane_regs.h:375-379` — so ADL-N uses the full 12 bits and
block index 4095 fits.

`[I915]`'s allowed DBUF-slice table for ADL-P/ADL-N (`display/skl_watermark.c:1144-1200`) says that
**a single active pipe gets all four slices**, and gives `PIPE_A` → `S1|S2|S3|S4` with
`join_mbus = true`. Slices are contiguous, so:

```
single pipe A, single plane:
    slice_mask  = S1|S2|S3|S4
    start = 0, end = 4096          -> PLANE_BUF_CFG = ((4096-1) << 16) | 0 = 0x0FFF0000
```

`[INF]` This is the *safe maximum*: the one plane owns the entire DBUF. It is not what a production
driver does (it wastes power), but it cannot under-allocate. For a first light-up, take it.

### 7.3 Watermarks — the simple version

`PLANE_WM(pipe, plane, level)` at `0x70240 + level*4`:
`PLANE_WM_EN[31]`, `PLANE_WM_IGNORE_LINES[30]`, `PLANE_WM_LINES[26:14]`, `PLANE_WM_BLOCKS[11:0]`.
`[I915]` `skl_universal_plane_regs.h:319-325`, `skl_universal_plane.c` `skl_plane_wm_reg_val`.

`PLANE_WM_EN` must be set for the levels you program. **Maximum `PLANE_WM_LINES` is 31** — a
hardware limit the PRM states explicitly, and if your calculated line count exceeds it, the level
is unusable and must be disabled. Note the field in `[I915]` is 13 bits wide
(`PLANE_WM_LINES_MASK = REG_GENMASK(26, 14)`), so the register will *accept* values well above 31;
the limit is a hardware constraint, not a field-width one. Do not let a successful read-back
convince you a too-large value is legal.

The PRM's level semantics: each level corresponds to a memory latency level. **If a latency level is
invalid, or if the maximum was exceeded for it or any earlier level, that level's `PLANE_WM_EN`
must be 0.** If level 0 itself exceeds the maximum, **the plane must not be enabled at all.**

`[INF]` Practical approach for a first bring-up, in order of increasing risk:

1. **Simplest that can work:** program level 0 only, with generous values
   (`BLOCKS` = the plane's whole DDB allocation, `LINES` = 31), `EN = 1`; disable levels 1..n.
   This over-allocates and may under-run on a busy memory system, but it is a valid starting point
   and it will produce pixels if everything else is right.
2. **Correct:** implement the PRM algorithm below and program every valid level.
3. Never: leave the watermark registers at their reset values.

### 7.4 The PRM watermark algorithm

Reproduced in condensed form from `[PRM]` "Watermark Algorithm". This is the authoritative
calculation; i915 implements a variant of it.

**Step 1 — retrieve memory latency.** Via the GT Driver Mailbox (PCode command `0x06`), two reads:

- Write `Data0 = 0`, `Data1 = 0`, command `0x06` → `Data0` gives levels 0–3 in bytes
  `[7:0], [15:8], [23:16], [31:24]` (microseconds).
- Write `Data0 = 1`, `Data1 = 0`, command `0x06` → `Data0` gives levels 4–7 likewise.
- Timeout **100 µs**; on timeout, **do not enable display planes**.
- *If level 1 or higher is `0x00`, that level and all higher levels are unused and invalid.*
- Programming note: *"If the mailbox response data for level 0 is 0us, add 2 microseconds to the
  result for each valid level."*

**Step 2 — adjusted pipe pixel rate.**
```
adjusted_pipe_rate = pixel_rate
if interlaced PF-ID:      *= 2
if pipe scaling enabled:  *= pipe_downscale
    pipe_downscale = max(1, hsrc/hdst) * max(1, vsrc/vdst)
```
**Step 3 — `WM_LINETIME`** (`0x45270` pipe A, `0x45274` pipe B; field `HSW_LINETIME[8:0]`):
```
line_time_us = roundup(8 * htotal / adjusted_pipe_rate_MHz)
```
`[I915]` `i915_reg.h:4382-4386`.

**Step 4 — per plane, per level.** With `bpp` = source bytes per pixel, `DBUF block size = 512`
bytes (256 for 8-bpp + Yf tiling):
```
blocks_per_line:
    linear or X tile:  ceil(bytes_per_line / 512) + 1
    Y tile:            ceil((min_scanlines_Y * bytes_per_line / 512) + 1) / min_scanlines_Y
                       where min_scanlines_Y = {1bpp:4, 2bpp:4, 4bpp:4, 8bpp:N/A} for 0/180 rotation
                                             {1bpp:16, 2bpp:8, 4bpp:4, 8bpp:N/A} for 90/270 rotation
method1 = (latency_us * adjusted_plane_rate_MHz * bpp / 512) + 1
method2 = ceil(latency_us * adjusted_plane_rate_MHz / htotal) * blocks_per_line
Y_tile_min = min_scanlines_Y * blocks_per_line
```
Selection (X tile / linear): if the plane's DDB allocation divided by `blocks_per_line` is ≥ 1 and
non-trivial, use `method2`; else if `latency_us >= line_time_us` use `method2`; else `method1`.
(Y tile: `max(method2, Y_tile_min)`.)

Then:
```
result_blocks = ceil(selected) + 1
result_lines  = ceil(selected / blocks_per_line)
```
Reject the level if `result_blocks >= plane_buffer_allocation`, or `result_lines > 31`.

**Step 5 — `WM_LINETIME` and `PLANE_WM` programming**, then write `PLANE_SURF` to arm.

**Step 6 — SAGV.** SAGV defaults **enabled** and can block display memory access during a voltage
transition. Requirements: calculate level 0 with `latency + SAGV_block_time`. If it fits, keep SAGV
on; if not, either redo all levels *without* the SAGV block time (which is what i915 effectively
does by sizing level 0 for it) or disable SAGV by masking PCode GV points.
`SAGV_block_time` comes from PCode command `0x23`.

`[INF]` For a first bring-up on one 1080p60 plane, the DDB is 4096 blocks and the plane needs on the
order of tens of blocks. Level 0 will fit comfortably whichever method you use. **Do not let SAGV
block the first light-up** — if the algorithm is intimidating, use the "generous level 0" approach
in §7.3 and revisit once pixels are on screen.

### 7.5 Reading the DRAM latency without PCode

`[I915]` `soc/intel_dram.c` reads DIMM geometry from **MCHBAR** (`MCHBAR` at PCI config `0x48` for
965+; `MCHBAR_I915 = 0x44`, `MCHBAR_SIZE = 16 KiB` — `[I915]` `intel_pci_config.h:36-38`), at
`SKL_MAD_DIMM_CH0_0_0_0_MCHBAR_MCMAIN` and `SKL_MAD_DIMM_CH1_0_0_0_MCHBAR_MCMAIN`, and uses
`snb_pcode_read(ICL_PCODE_MEM_SUBSYSYSTEM_INFO | ICL_PCODE_MEM_SS_READ_GLOBAL_INFO)` for the rest.
`[I915]` `soc/intel_dram.c:386,392,589-590`; PCode constants at `i915_reg.h:3516-3520`.

`[GAP]` On ADL-N the memory is soldered LPDDR5 and there are no DIMMs, so the 16 Gb-DIMM level-0
adjustment (`[PRM]` "Level 0 Adjustment for 16Gb DIMMs", MCHBAR `+0x500C`/`+0x5010`) is
probably irrelevant. I could not source whether ADL-N needs an equivalent adjustment for LPDDR5.
**Treat this as an open question** — it would show up as marginal underruns at high resolutions, not
as a dead screen.

---

## 8. DDI and PHY

### 8.1 Which ports exist on ADL-N

`[I915]` `xe_lpd_display` declares `port_mask = PORT_A | PORT_B | PORT_TC1 | PORT_TC2 | PORT_TC3 |
PORT_TC4` (`intel_display_device.c:1083-1085`). But that is the *maximum* `XE_LPD` configuration, and
i915's own comment for DG1 says *"Some SKUs will limit which ports are connected in the die and
package. Software should rely on hotplug to determine which ports are actually available."*
`[PRM]` "Port Availability".

`[I915]`'s internal port numbering is confusing and you must know it to read the offsets:

```
PORT_A = 0, PORT_B = 1, PORT_C = 2, PORT_D = 3, PORT_E = 4, ...
PORT_TC1 = PORT_D = 3, PORT_TC2 = 4, PORT_TC3 = 5, PORT_TC4 = 6
PORT_D_XELPD = PORT_TC5 = 7, PORT_E_XELPD = PORT_TC6 = 8
```
`[I915]` `display/intel_display_limits.h:82-108`.

So "TC1" and "D" are the same enum value; the `_PORT(port, a, b)` register macro uses the *numeric*
value and only distinguishes A from B (`_PICK_EVEN`), because the higher ports live in different
blocks entirely (combo PHY / DKL PHY).

`[I915]` `intel_encoder_is_combo` / `intel_encoder_is_dkl` decide which PHY family a port uses, and
that decision drives which buf-translation table and which PLL apply
(`display/intel_ddi_buf_trans.c:1700-1725`). `[GFXINIT]` independently reports that ADL-N's rear
HDMI is on a **combo PHY** and that DP++ is on **TC1** — so both families exist on at least some
ADL-N boards.

**For a first bring-up, use a combo PHY port (A or B).** Combo PHY programs are self-contained and
do not require the Type-C/FIA machinery.

### 8.2 Combo PHY registers

Combo PHY A base `0x162000`, combo PHY B base `0x06C000`. `[I915]`
`display/intel_combo_phy_regs.h:11-18`. (PHY C/EHL `0x160000`, PHY D/RKL `0x161000`, PHY E/ADL-S
`0x16B000` — none of these are known to exist on ADL-N.)

Sub-blocks within a PHY. Every combo PHY register is `PHY_BASE + SUB_BLOCK(instance) + 4 * dw`.
There are four blocks (`CL`, `COMP`, `PCS`, `TX`) and **eight instances** in total, named
`_ICL_PORT_<block>_<instance>`:

| Sub-block | `AUX` | `GRP` (group) | `LN(ln)` (per lane) | What it holds |
|---|---|---|---|---|
| `CL` | *(implicit `+0x000`)* | — | — | `DW5` `CL_POWER_DOWN_ENABLE[4]`, `SUS_CLOCK_CONFIG[1:0]`; `DW10` `PWR_DOWN_LN_MASK[7:4]`; `DW12` `LANE_ENABLE_AUX[0]` |
| `COMP` | — | `+0x100` | — | `DW0` `COMP_INIT[31]`; `DW1`/`DW9`/`DW10` = procmon; `DW3` process/voltage; `DW8` `IREFGEN[24]` |
| `PCS` | `+0x300` | `+0x600` | `+0x800 + ln*0x100` | `DW1` `DCC_MODE_SELECT` |
| `TX` | `+0x380` | **`+0x680`** | `+0x880 + ln*0x100` | `DW2` swing, `DW4` cursor coeff, `DW5` training enable / scaling mode, `DW7` N scalar, `DW8` ODCC |

Offsets are defined in `[I915]` `display/intel_combo_phy_regs.h:24` (`CL`), `:50` (`COMP`),
`:77-79` (`PCS`) and `:96-98` (`TX`).

**Two mnemonics make this recomputable rather than memorisable:**

1. **`TX` is always exactly `+0x80` after the corresponding `PCS` instance.** AUX: `0x300`/`0x380`.
   Group: `0x600`/`0x680`. Lane: `0x800`/`0x880`. Find one and you can derive the other.
2. **`CL` and `COMP` are group-wide only** — no AUX or per-lane instances — which is why
   `_ICL_PORT_CL_DW(dw, phy) = PHY_BASE + 4*dw` has no sub-block term at all.

Worked examples for **combo PHY A** (`PHY_BASE = 0x162000`):

| Register | Derivation | Absolute |
|---|---|---|
| `ICL_PORT_COMP_DW0(A)` | `0x162000 + 0x100 + 4*0` | `0x162100` |
| `ICL_PORT_CL_DW5(A)` | `0x162000 + 0x000 + 4*5` | `0x162014` |
| `ICL_PORT_PCS_DW1_GRP(A)` | `0x162000 + 0x600 + 4*1` | `0x162604` |
| `ICL_PORT_TX_DW2_GRP(A)` | `0x162000 + 0x680 + 4*2` | `0x162688` |
| `ICL_PORT_TX_DW4_GRP(A)` | `0x162000 + 0x680 + 4*4` | `0x162690` |
| `ICL_PORT_TX_DW5_GRP(A)` | `0x162000 + 0x680 + 4*5` | `0x162694` |
| `ICL_PORT_TX_DW8_GRP(A)` | `0x162000 + 0x680 + 4*8` | `0x1626A0` |

> **Correction to an earlier draft.** This table previously gave the `PORT_TX_DW*` group base as
> `+0x400`. **That is wrong.** `+0x400` is not any defined sub-block and lands in unassigned space
> between `PCS_AUX` (`0x300`) and `PCS_GRP` (`0x600`); a buffer-translation value written there
> would reach no register, and the swing/pre-emphasis programming would silently do nothing — which
> presents as a link that trains at the wrong level or not at all, *after* the PLL has already
> locked, so it looks like a PHY or cable problem rather than an offset bug. The group base is
> **`+0x680`**.

> **`ICL_PHY_MISC` is NOT in the combo PHY aperture.** It lives at `0x64C00` (PHY A) and `0x64C04`
> (PHY B) — `[I915]` `i915_reg.h:4458-4459` — in the DDI register block, nowhere near `0x162000` or
> `0x06C000`. It is the one combo-PHY-related register you **cannot** derive from `PHY_BASE`.
> Fields: `DE_IO_COMP_PWR_DOWN`, `MUX_DDID`.

`[I915]` `intel_combo_phy_regs.h:20-105`, `i915_reg.h:4453-4459`.

### 8.3 Combo PHY initialisation

`[PRM]` "Combo PHY Initialization Sequence" and `[I915]` `icl_combo_phys_init`
(`display/intel_combo_phy.c:308-380`) agree step for step. Follow this **before** enabling the
PHY's DDI-IO or AUX power:

```
1. PORT_TX_DW8:  set odcc_clk_sel, odcc_clk_div_sel = divide by 2
   PORT_PCS_DW1: set DCC Mode Select = RUN_DCC_ONCE   (Gen12; "DCC continuous mode" in older text)
2. if PORT_COMP_DW0.COMP_INIT == 1: already initialised, skip the rest
3. ICL_PHY_MISC: clear DE_IO_COMP_PWR_DOWN
4. PROGRAM PROCMON  (see table below)
5. if this PHY is a comp source: PORT_COMP_DW8 |= IREFGEN
6. PORT_COMP_DW0 |= COMP_INIT
7. PORT_CL_DW5 |= CL_POWER_DOWN_ENABLE
```

`[I915]` does steps 1 and 2 only for `DISPLAY_VER >= 12`, which includes ADL-N — the `[PRM]` text
for DG1 (display 12) matches.

#### The procmon table

`[PRM]` "Procmon Reference Values" and `[I915]` `icl_procmon_values[]`
(`display/intel_combo_phy.c:28-52`) give **identical** values. This is a strong mutual
verification.

| Voltage / process | `PORT_COMP_DW1[7:0]` | `PORT_COMP_DW1[23:16]` | `PORT_COMP_DW9` | `PORT_COMP_DW10` |
|---|---|---|---|---|
| 0.85 V dot-0 | `0x00` | `0x00` | `0x62AB67BB` | `0x51914F96` |
| 0.95 V dot-0 | `0x00` | `0x00` | `0x86E172C7` | `0x77CA5EAB` |
| 0.95 V dot-1 | `0x00` | `0x00` | `0x93F87FE1` | `0x8AE871C5` |
| 1.05 V dot-0 | `0x00` | `0x00` | `0x98FA82DD` | `0x89E46DC1` |
| 1.05 V dot-1 | `0x00` | `0x44` | `0x9A00AB25` | `0x8AE38FF1` |

Select by reading `PORT_COMP_DW3`: `PROCESS_INFO_MASK = [28:26]` (dot-0 = 0, dot-1 = 1, dot-4 = 2)
and `VOLTAGE_INFO_MASK = [25:24]` (0.85 V = 0, 0.95 V = 1, 1.05 V = 2). `[I915]`
`intel_combo_phy_regs.h:55-62`, `intel_combo_phy.c:54-75`.

`PORT_COMP_DW1` is written with a **masked** write: `rmw(DW1, (0xff<<16)|0xff, procmon->dw1)`.
`PORT_COMP_DW9`/`DW10` are full 32-bit writes. `[I915]` `icl_set_procmon_ref_values`.

#### Comp source / sink

`[PRM]` "Combo PHY Comp Sources": **PHY A is the comp source for PHY B** (and, historically, for the
DPLLs). A comp source must be initialised before its sinks, and must stay initialised while the
sinks are in use. `[I915]` `phy_is_master` returns true for `PHY_A` unconditionally, and for ADL-P/N
returns false for everything else (`display/intel_combo_phy.c:186-210`). **So on ADL-N, only PHY A
gets `IREFGEN`.**

This has a practical consequence: if you initialise only PHY B and skip PHY A, compensation is
wrong. **Initialise all combo PHYs present, PHY A first.**

### 8.4 DDI registers

| Register | Port A | Port B | Layout |
|---|---|---|---|
| `DDI_BUF_CTL` | `0x64000` | `0x64100` | `ENABLE[31]`, `BUF_TRANS_SELECT[27:24]`, `PHY_LINK_RATE[23:20]`, `PORT_REVERSAL[16]`, `IS_IDLE[7]`, `A_4_LANES[4]`, `PORT_WIDTH[3:1]` |
| `DDI_BUF_TRANS_LO(port,i)` | `0x64E00 + i*8` | `0x64E60 + i*8` | de-emphasis / balance leg |
| `DDI_BUF_TRANS_HI(port,i)` | `+4` | `+4` | vref / vswing |
| `DP_AUX_CH_CTL` | `0x64010` | `0x64110` | see §11 |
| `DP_AUX_CH_DATA(i)` | `0x64014 + i*4` | `0x64114 + i*4` | 5 registers |
| `TRANS_DDI_FUNC_CTL` | per transcoder, `0x60400`+ | | see below |

`[I915]` `i915_reg.h:3854-3882`, `intel_dp_aux_regs.h:24-27,78-81`.

`TRANS_DDI_FUNC_CTL` for transcoder T is at `0x60400 + T*0x1000` (transcoder A = `0x60400`).
`[I915]` `i915_reg.h:3740-3760`. Fields, Gen12 encodings:

| Field | Bits | Notes |
|---|---|---|
| `TRANS_DDI_FUNC_ENABLE` | 31 | |
| `TGL_TRANS_DDI_PORT_MASK` | `[30:27]` | `TGL_TRANS_DDI_SELECT_PORT(p) = (p+1) << 27` |
| `TRANS_DDI_MODE_SELECT_MASK` | `[26:24]` | **HDMI = 0, DVI = 1, DP_SST = 2, DP_MST = 3**, FDI/128b132b = 4 |
| `TRANS_DDI_BPC_MASK` | `[22:20]` | 8 bpc = 0, 10 bpc = 1, 6 bpc = 2, 12 bpc = 3 |
| `TRANS_DDI_PVSYNC` / `PHSYNC` | **17 / 16** | from the mode's polarity flags |
| `TRANS_DDI_PORT_SYNC_MASTER_SELECT` | `[19:18]` | port-sync only |
| `TRANS_DDI_PORT_WIDTH_MASK` | `[3:1]` | `(lanes - 1) << 1` |
| `TRANS_DDI_HIGH_TMDS_CHAR_RATE` | **4** | needed for TMDS ≥ 340 MHz |
| `TRANS_DDI_HDMI_SCRAMBLING` | **0** | needed for HDMI ≥ 340 MHz |

`[I915]` `i915_reg.h:3748-3800`, `intel_ddi.c:471-580`.

> **Correction to an earlier draft.** Four rows of this table were wrong there: the mode-select
> values (`DP_SST`/`DP_MST` are 2/3, not 4/5), the polarity bits (17/16, not 19/18 — bits 19:18 are
> the port-sync master select), and both HDMI high-rate bits (4 and 0, not 11 and 12). Confirmed at
> `[I915]` `i915_reg.h:3758-3800`.

**Where the output bit depth actually goes.** `TRANS_DDI_FUNC_CTL` does carry a BPC field, but on
Gen12 the *pipe-side* output depth and dithering live in **`PIPE_MISC`** (`0x70030`), not in
`TRANSCONF`:

| Field | Bits | Notes |
|---|---|---|
| `PIPE_MISC_BPC_MASK` | `[7:5]` | 8 bpc = 0, 10 bpc = 1, 6 bpc = 2, 12 bpc = 4 (**ADL-P+ only**) |
| `PIPE_MISC_DITHER_ENABLE` | 4 | |
| `PIPE_MISC_DITHER_TYPE` | `[3:2]` | spatial / ST1 / ST2 / temporal |

`[I915]` `i915_reg.h:1708-1740`, including the source comment confirming that for `ADLP+` bits
`[7:5]` are *port output BPC* rather than the pre-Gen13 *dither* BPC. **An earlier draft attributed
these to `TRANSCONF`; that was wrong** — `TRANSCONF`'s BPC/DITHER fields there are a pre-Haswell
leftover and are not what Gen12 uses.

`[INF]` **HDMI ≥ 300 MHz TMDS (approximately 4K30 or 1080p with high pixel clock) requires
scrambling and the high TMDS character rate.** For a first 1080p60 (148.5 MHz) mode, neither is
needed. Do not enable them until the simple case works.

### 8.5 DDI buffer translation (voltage swing / pre-emphasis)

#### Which table

`[I915]` `intel_ddi_buf_trans_init` (`display/intel_ddi_buf_trans.c:1699-1730`):

- ADL-P/N (`IS_ALDERLAKE_P`), **combo** encoder → `adlp_get_combo_buf_trans`
- ADL-P/N, DKL encoder → `adlp_get_dkl_buf_trans`

and, within the combo path (`intel_ddi_buf_trans.c:1585-1626`):

| Condition | Table |
|---|---|
| HDMI | `icl_combo_phy_trans_hdmi` |
| eDP, port clock > 540 MHz | `adlp_combo_phy_trans_edp_hbr3` |
| eDP, HOBL | `tgl_combo_phy_trans_edp_hbr2_hobl` |
| eDP, low vswing | `adlp_combo_phy_trans_edp_up_to_hbr2` |
| eDP otherwise / DP, clock > 270 MHz | `adlp_combo_phy_trans_dp_hbr2_hbr3` |
| DP, clock ≤ 270 MHz | `adlp_combo_phy_trans_dp_hbr` |

**For an HDMI monitor you want `icl_combo_phy_trans_hdmi`** — the ICL table, reused unchanged on
ADL-P/N. `[GAP]` I did not extract its values; they are in `intel_ddi_buf_trans.c` and are short.
For **DisplayPort**, the full ADL-P combo DP tables are reproduced below because they are the ones
you need and they differ from DG1's.

#### ADL-P/N combo PHY, DP up to HBR (≤ 270 MHz), 10 entries

`[I915]` `display/intel_ddi_buf_trans.c:882-894`. Fields are
`{dw2_swing_sel, dw7_n_scalar, dw4_cursor_coeff, dw4_post_cursor_2, dw4_post_cursor_1}`
(`intel_ddi_buf_trans.h:28-34`). Index = `DDI_BUF_TRANS_SELECT` value in `DDI_BUF_CTL[27:24]`.

| Idx | DW2 | DW7 | DW4 | post2 | post1 | Non-trans → Trans mV | DE dB |
|---|---|---|---|---|---|---|---|
| 0 | `0xA` | `0x35` | `0x3F` | `0x00` | `0x00` | 350 → 350 | 0.0 |
| 1 | `0xA` | `0x4F` | `0x37` | `0x00` | `0x08` | 350 → 500 | 3.1 |
| 2 | `0xC` | `0x71` | `0x31` | `0x00` | `0x0E` | 350 → 700 | 6.0 |
| 3 | `0x6` | `0x7F` | `0x2C` | `0x00` | `0x13` | 350 → 900 | 8.2 |
| 4 | `0xA` | `0x4C` | `0x3F` | `0x00` | `0x00` | 500 → 500 | 0.0 |
| 5 | `0xC` | `0x73` | `0x34` | `0x00` | `0x0B` | 500 → 700 | 2.9 |
| 6 | `0x6` | `0x7F` | `0x2F` | `0x00` | `0x10` | 500 → 900 | 5.1 |
| 7 | `0xC` | `0x7C` | `0x3C` | `0x00` | `0x03` | 650 → 700 | 0.6 |
| 8 | `0x6` | `0x7F` | `0x35` | `0x00` | `0x0A` | 600 → 900 | 3.5 |
| 9 | `0x6` | `0x7F` | `0x3F` | `0x00` | `0x00` | 900 → 900 | 0.0 |

For HBR2/HBR3 (> 270 MHz) use `_adlp_combo_phy_trans_dp_hbr2_hbr3`
(`intel_ddi_buf_trans.c:901-912`), which differs in entries 2, 3, 6, 7, 8.

#### Writing the table

`[PRM]` "Voltage Swing Programming Sequence":

```
1. PORT_PCS_DW1: cmnkeeper_enable = 1 for eDP/DP, 0 otherwise
2. PORT_TX_DW4 per-lane loadgen select  (NOT group access — each lane differs)
     bit rate <= 6 GHz, 4 lanes:  ln0=0 ln1=1 ln2=1 ln3=1
     bit rate <= 6 GHz, 1-2 lanes: ln0=0 ln1=1 ln2=1 ln3=0
     bit rate  > 6 GHz:            all 0
3. PORT_CL_DW5 SUS Clock Config = 0b11
4. PORT_TX_DW5 TX Training Enable = 0
5. PORT_TX_DW5 Scaling Mode Sel = 0b010
   program PORT_TX_DW2, PORT_TX_DW4, PORT_TX_DW5, PORT_TX_DW7 from the table
6. PORT_TX_DW5 TX Training Enable = 1   (triggers the update)
```

The `[PRM]` note is explicit that **group access must not be used for step 2**, and that later writes
to `PORT_TX_DW4` must also avoid group access so they do not clobber the per-lane values.

**Source disagreement, recorded:** the DG1 PRM's own voltage-swing table
(`[PRM]` "Voltage Swing Programming") gives *different* values from i915's ADL-P table for the same
nominal levels — e.g. DG1 level 0/0 is `DW2 = 4'b1010, DW7 = 0x32, DW4 = 0x3F` while ADL-P's is
`DW2 = 0xA, DW7 = 0x35, DW4 = 0x3F`; DG1 level 1/1 is `DW7 = 0x48` vs ADL-P's `0x4F`. **I trust the
ADL-P table**, because it is the table i915 selects for exactly this platform; the DG1 table is for
a different board with different package parasitics. The swing values are board-tuned, which is
precisely why they differ between two parts with the same PHY IP.

### 8.6 HDMI/DVI bring-up

`[PRM]` "Sequences for HDMI and DVI" — enable sequence. Merged with the Gen12-correct pieces from
`[I915]` `tgl_ddi_pre_enable_dp` / `intel_ddi_pre_enable_hdmi`
(`display/intel_ddi.c:2853-2876`, `2620-2740`):

```
1. Enable power wells:  PW_1, PW_2, PW_A (if pipe A), DDI_IO_<port>, AUX_<port>
2. Combo PHY init (once, §8.3)
3. Enable & lock the port PLL (DPLL0/1, §6.3)
4. Program DDI->PLL mapping (DPCLKA_CFGCR0) and clear DDI_CLK_OFF
   -- two separate writes, per the spec note
5. Enable DDI IO power: set REQ in ICL_PWR_WELL_CTL_DDI2, poll STATE
   [PRM] timeout 20us
6. Program voltage swing / buffer translation (§8.5)
7. POWER UP THE LANES: PORT_CL_DW10 PWR_DOWN_LN_MASK
     HDMI/DVI: 4 lanes -> PWR_UP_ALL_LANES (0x0)
                2 lanes -> PWR_DOWN_LN_3_2 (0xC)
                1 lane  -> PWR_DOWN_LN_3_2_1 (0xE)
8. Program TRANS_CLK_SEL(tran) = TGL_TRANS_CLK_SEL_PORT(phy)   (§6.3: PHY, not port, on ver 13)
9. Program transcoder timings (§6.1) and PIPESRC
10. Program & enable the plane(s)     (order: WM/DDB -> ... -> PLANE_CTL -> PLANE_SURF)
11. TRANS_DDI_FUNC_CTL = ENABLE | SELECT_PORT | MODE_SELECT_HDMI/DVI
                         | BPC_8 | polarity | (scrambling if needed)
12. TRANSCONF (0x70008) = ENABLE | progressive | 8bpc   (bit 30 is the hardware's status, not a
    request -- see the note under phase 5.6)
13. DDI_BUF_CTL = ENABLE | BUF_TRANS_SELECT(level) | PHY_LINK_RATE(rate)
                  | PORT_WIDTH(lanes-1) | A_4_LANES if 4
14. Poll DDI_BUF_CTL.IS_IDLE == 0   ("not idle")   [PRM] timeout 500us for HDMI
```

`[I915]` encodes `PWR_DOWN_LN_MASK` in `PWR_DOWN_LN_*` constants
(`display/intel_combo_phy_regs.h:29-40`) and the rate encoding in `ddi_buf_phy_link_rate`
(`intel_ddi.c:302-322`): `162000→0, 216000→4, 243000→5, 270000→1, 324000→6, 432000→7, 540000→2,
810000→3`. **Note this is not a monotonic encoding** — 270 MHz is 1 and 810 MHz is 3 while
324 MHz is 6.

#### The disable sequence (memorise this — you will use it constantly)

`[PRM]` "Disable Sequence", HDMI/DVI:

```
1. Disable planes  ->  PLANE_CTL = 0, PLANE_SURF = 0
2. Disable TRANSCONF; poll for off state, timeout two frame times
3. TRANS_DDI_FUNC_CTL: clear ENABLE, set DDI_Select = None
4. DDI_BUF_CTL = 0; wait 8us or poll IS_IDLE for buffers to return to idle
5. TRANS_CLK_SEL = disabled
6. clear DDI IO power request
7. clear DDI->PLL mapping; disable the PLL if unused
8. disable the power wells that are no longer needed
```

Order matters in both directions: **planes → pipe/transcoder → DDI → clock → PHY → power**, and the
exact reverse to enable. Getting this backwards on *disable* is how you get an unkillable underrun.

### 8.7 eDP (only if the machine has an internal panel)

Skip this entirely if the target is an external monitor. If not:

1. **Panel power sequencing** — `PP_STATUS` `0x61200`, `PP_CONTROL` `0x61204`,
   `PP_ON_DELAYS` `0x61208`, `PP_OFF_DELAYS` `0x6120C`, `PP_DIVISOR` `0x61210`.
   `[I915]` `display/intel_pps_regs.h:13-77`.
   **The PPS base differs by platform.** `[I915]` `intel_pps_setup` (`display/intel_pps.c:1710-1720`)
   selects `PCH_PPS_BASE = 0xC7200` only for `HAS_PCH_SPLIT || GLK || BXT`, and `PPS_BASE = 0x61200`
   otherwise. ADL-N is not in the first set, so **ADL-N uses `0x61200`**.
   `[INF]` from `[I915]`; `[PRM]` lists `PP_STATUS`/`PP_CONTROL` without giving an address in the
   text I extracted, so this rests on i915 alone.

   The handshake: set `PP_CONTROL.PANEL_POWER_ON`, poll `PP_STATUS.PP_ON` and then
   `PP_STATUS.PP_READY` (which requires PLL + pipe + port all on), and set
   `PP_CONTROL.EDP_BLC_ENABLE` for the backlight. Off is the reverse with the programmed delays.
   Also: **`PP_CONTROL` is write-protected** — `PANEL_UNLOCK_REGS` (`0xABCD` in `[31:16]`) must be
   written first. `[I915]` `intel_pps_regs.h:49-50`.

2. **The panel's timings come from the VBT**, not from EDID, unless you deliberately ignore the VBT
   and use the panel's EDID (which eDP panels do provide in recent designs). `[I915]`
   `display/intel_bios.c`. `[GAP]` I did not extract the VBT child-device structure; it is large and
   I judged it out of scope for a first bring-up on an external monitor.

3. **AUX** for eDP is the same block as DP AUX (§9.6).

`[INF]` **Defer eDP.** It adds panel power sequencing, backlight, VBT parsing and PSR, for no
benefit to the stated goal of "a kernel-provided linear framebuffer scanned out on a real monitor".

### 8.8 Type-C / DKL PHY — defer

The Type-C path adds: FIA configuration (`DFLEXDPSP`, `DFLEXDPMLE`), the TC port state machine
(`[I915]` `display/intel_tc.c`), a separate DKL PLL family with different registers
(`PORTTC1_PLL_ENABLE` `0x46038`), and a separate buffer-translation table family. None of it is
needed to light up a combo-PHY HDMI port.

`[I915]` `tgl_ddi_pre_enable_dp` step 5 shows that IO power for a Type-C port can be gated behind
the TBT-alt-mode check — one of several places where the Type-C path forks. **Do not start here.**

---

## 9. GMBUS, EDID and hotplug

### 9.1 The GMBUS block

GMBUS is Intel's I2C master for DDC (EDID) and for DPCD over I2C (not AUX). On ADL-N it lives in
the south display window at base **`0xC0000`** (`[I915]` `display/intel_gmbus.c:871-879` sets
`gmbus.mmio_base = PCH_DISPLAY_BASE` for every platform that is neither VLV/CHV nor GMCH).

| Register | Address | Purpose |
|---|---|---|
| `GPIO(n)` | `0xC5010 + 4n` | GPIO direction/value/pullup |
| `GMBUS0` | `0xC5100` | clock/port select, rate, byte-count override |
| `GMBUS1` | `0xC5104` | command/status, cycle type, slave address, byte count |
| `GMBUS2` | `0xC5108` | status |
| `GMBUS3` | `0xC510C` | data buffer (4 bytes) |
| `GMBUS4` | `0xC5110` | interrupt mask |
| `GMBUS5` | `0xC5120` | 2-byte index enable |

`[I915]` `display/intel_gmbus_regs.h:13-79`.

### 9.2 The pin map

GMBUS attaches to a GPIO pin **pair**, selected by the pin-index field of `GMBUS0`.
`[TGL12]` "GMBUS and GPIO" describes the field as `GMBUS0[4:0]`, with values **1–4 = DDC for DDI
A/B/C/D** and 0 = none; `[PRM]` "Pin Usage" gives the same 1-based mapping. `[I915]` agrees: the
pin constants are `GMBUS_PIN_1_BXT = 1`, `_2_BXT = 2`, `_3_BXT = 3`, and the ICP pin table maps
those to DDC A, B, C.
`[I915]` `display/intel_gmbus.h:23-33`, `display/intel_gmbus.c:114-125`.

**The pin index is 1-based. DDI A is pin value 1, not 0.** An earlier draft of this document had
this as 0-based; it was wrong.

| `GMBUS0` pin index | Constant | DDC for | GPIO |
|---|---|---|---|
| 0 | — | *none/disconnected* | — |
| **1** | `GMBUS_PIN_1_BXT` | DDI **A** | `GPIOB` |
| **2** | `GMBUS_PIN_2_BXT` | DDI **B** | `GPIOC` |
| 3 | `GMBUS_PIN_3_BXT` | DDI C | `GPIOD` |
| 4 | `GMBUS_PIN_4_CNP` | DDI D | `GPIOE` |
| 9 | `GMBUS_PIN_9_TC1_ICP` | TC1 | `GPIOJ` |
| 10 | `GMBUS_PIN_10_TC2_ICP` | TC2 | `GPIOK` |
| 11 | `GMBUS_PIN_11_TC3_ICP` | TC3 | `GPIOL` |
| 12 | `GMBUS_PIN_12_TC4_ICP` | TC4 | `GPIOM` |

The rows above pin 4 are the ICP+ table; `[I915]` selects it for ADL-N via
`INTEL_PCH_TYPE >= PCH_ICP`. `[INF]` on that selection — see §13.2.

**Important caveat on `GMBUS1`–`GMBUS4`:** they are **not defined in any public Intel Gen12 register
volume.** The TGL 2c, DG1 2c and RKL Vol 2 PDFs were all grepped: only `GMBUS0` is present (DG1 also
mentions "GMBUS4 Interrupt Mask" in passing). **The entire transaction protocol below therefore
rests on `[I915]` alone.** That is a real single-source risk and it is recorded in §13.1.

### 9.3 The transaction protocol

`GMBUS1` fields: `SW_CLR_INT[31]`, `SW_RDY[30]`, `ENT[29]` (enable timeout),
`CYCLE[27:25]` (`NONE=0`, `WAIT=1`, `INDEX=2`, `STOP=4`),
`BYTE_COUNT[23:16]` (max 511 on Gen9+, `GEN9_GMBUS_BYTE_COUNT_MAX`), `SLAVE_INDEX[15:8]`,
`SLAVE_ADDR[7:1]`, `SLAVE_READ[0]`. `[I915]` `intel_gmbus_regs.h:40-54`.

`GMBUS2` status bits: `INUSE[15]`, `HW_WAIT_PHASE[14]`, `STALL_TIMEOUT[13]`, `INT[12]`,
`HW_RDY[11]`, `SATOER[10]` (secondary address / NAK timeout error), `ACTIVE[9]`.
`[I915]` `intel_gmbus_regs.h:57-64`.

**Reading an EDID block** (`[I915]` `gmbus_xfer`/`gmbus_xfer_read`, `display/intel_gmbus.c:429-680`):

```
1. GMBUS0 = rate | pin_index            (rate: 0=100kHz, 1=50kHz, 2=400kHz, 3=1MHz)
2. GMBUS5 = GMBUS_2BYTE_INDEX_EN        (for a 2-byte register index; EDID uses 1 byte)
3. GMBUS1 = CYCLE_WAIT | (len << 16) | (slave_addr << 1) | SLAVE_READ | SW_RDY
4. loop len/4 times:
       poll GMBUS2.HW_RDY == 1
       if GMBUS2.SATOER: abort with -ENXIO
       read 4 bytes from GMBUS3 (little-endian: byte0 = bits[7:0])
5. GMBUS0 = 0    (release)
```

The two-step EDID read is:
- **Write** the 1-byte register address (`0x00`) to slave `0x50` with `CYCLE_INDEX` (this is the
  "no-stop" index cycle: `GMBUS_CYCLE_INDEX = 2<<25`).
- **Read** N bytes from slave `0x50` with `CYCLE_WAIT` and a terminating stop.

`[I915]` combines these: when the first message has no `I2C_M_NOSTART`, it sets
`gmbus1_index = GMBUS_CYCLE_INDEX | (msgs[0].len << 16) | (addr << 1) | SW_RDY` for the *first*
message and then issues the read with `GMBUS_CYCLE_WAIT`. `[I915]`
`display/intel_gmbus.c:600-660`.

**Burst reads > 511 bytes** follow a different, documented path (`[PRM]` "Sequence for GMBUS Burst
Reads Greater Than 511 Bytes"): set `GMBUS0.BYTE_CNT_OVERRIDE[6]`, program
`total = X - N*256` where `N = INT(X/256) - 1`, read `N*256 + 4` bytes, clear the override, read the
rest. i915 implements this with a 512-byte special case that reads one extra byte and discards it
(`[I915]` `gmbus_xfer_read_chunk`). **EDID is 128 or 256 bytes, so you will not hit this** unless
you read the 512-byte E-EDID extension block — which you might, for CEA-861 extensions
(HDMI audio, speaker allocation, and the 4K modes). Budget for it.

### 9.4 Detecting a connected sink

Two independent signals:

1. **Hotplug (HPD)**, §9.5 — the authoritative "something is plugged in".
2. **`DDI_BUF_CTL.DDI_INIT_DISPLAY_DETECTED[0]`** — a legacy DVI/HDMI-only presence detect.
   `[I915]` `i915_reg.h:3875`. `[INF]` Do not rely on it for DP; on modern parts HPD is the signal.

The PRM's advice (`[PRM]` "Interrupts and Hot Plug") is the right procedure:

> *"To find if a receiver was connected before hotplug was enabled, enable hotplug in SHOTPLUG_CTL
> and then read the interrupt ISR to find the live connect state."*

i.e. **enable HPD, then read the status register once** — that read gives you the current live
state, which is how you discover a monitor that was already plugged in at boot.

### 9.5 Hotplug registers

`[I915]` `i915_reg.h:3075-3090`:

| Register | Address | Purpose |
|---|---|---|
| `SHOTPLUG_CTL_DDI` | `0xC4030` | DDI A/B/C/D hotplug control + status |
| `SHOTPLUG_CTL_TC` | `0xC4034` | Type-C hotplug control |
| `SHPD_FILTER_CNT` | `0xC4038` | pulse filter |

`SHOTPLUG_CTL_DDI` is arranged 4 bits per DDI, where the DDI index is
`_HPD_PIN_DDI(hpd_pin) = hpd_pin - HPD_PORT_A` (`[I915]` `i915_reg.h:2543`):

| Field | Value | Meaning |
|---|---|---|
| `HPD_ENABLE` | `0x8 << (idx*4)` | enable detection |
| `HPD_OUTPUT_DATA` | `0x4 << (idx*4)` | drive the HPD pin |
| `HPD_STATUS_MASK` | `0x3 << (idx*4)` | 0 = no detect, 1 = short, 2 = long, 3 = short|long |

Long-duration detect is a real monitor connect; short is typically a pulse. Program
`HPD_LONG_DETECT` (2) unless you have a reason to want both.

`SHPD_FILTER_CNT` values `0x001D9` (500 µs adjusted) and `0x000F8` (250 µs) are given in
`[I915]` `i915_reg.h:3091-3093`.

**Board inversion.** `[PRM]` warns: *"The hotplug level shifter on the board inverts the hotplug so
that connect=0 and disconnect=1. Register 0xC2000 bits 18:15 must be set to 1111b before enabling
hotplug to account for the board inversion."* `[INF]` This is DG1-board-specific and I could not
confirm it applies to any ADL-N board. **If your hotplug polarity is inverted, this is why** — check
`0xC2000[18:15]`. `[GAP]` I did not identify what `0xC2000` is named in i915; it is not a register
i915 programs for this purpose.

### 9.6 AUX (DisplayPort only)

Skip unless you are doing DP. `[I915]` `display/intel_dp_aux_regs.h`:

| Register | Port A | Port B |
|---|---|---|
| `DP_AUX_CH_CTL` | `0x64010` | `0x64110` |
| `DP_AUX_CH_DATA(i)`, i=0..4 | `0x64014 + 4i` | `0x64114 + 4i` |

`DP_AUX_CH_CTL`: `SEND_BUSY[31]`, `DONE[30]`, `INTERRUPT[29]`, `TIME_OUT_ERROR[28]`,
`TIME_OUT[27:26]`, `RECEIVE_ERROR[25]`, `MESSAGE_SIZE[24:20]`, `TBT_IO[11]`.
`[I915]` `intel_dp_aux_regs.h:46-72`. Max AUX payload is **20 bytes**, hence the 5 data registers.

The AUX power well must be enabled first (§4.2, `AUX_A` / `AUX_B`).

---

## 10. Interrupts

### 10.1 Why you need them for a first modeset

Not for the pixels — for **knowing when to stop**. You need:

- **vblank** to confirm the pipe is actually running and to pace a mode set,
- **hotplug** to detect the monitor,
- **underrun / error** because they are the fastest signal that your watermarks are wrong.

### 10.2 The display interrupt register set

`[I915]` `i915_reg.h:2457-2460`, `2499-2502`, `2546-2549`, `2578`:

| Register | ISR | IMR | IIR | IER |
|---|---|---|---|---|
| Display engine master | `DEISR 0x44000` | `DEIMR 0x44004` | `DEIIR 0x44008` | `DEIER 0x4400C` |
| Per pipe (A–D) | `GEN8_DE_PIPE_ISR(pipe) = 0x44400 + pipe*0x10` | `+4` | `+8` | `+0xC` |
| Port | `GEN8_DE_PORT_ISR 0x44440` | `0x44444` | `0x44448` | `0x4444C` |
| Misc | `GEN8_DE_MISC_ISR 0x44460` | `0x44464` | `0x44468` | `0x4446C` |
| South display | `SDEISR 0xC4000` | `SDEIMR 0xC4004` | `SDEIIR 0xC4008` | `SDEIER 0xC400C` |

Semantics (standard Intel): **ISR** = live status, read-only. **IMR** = mask; **set = masked
(disabled)**. **IIR** = write-1-to-clear; reading gives the currently pending *unmasked* interrupts.
**IER** = enable; **set = enabled**.

### 10.3 The global display interrupt enable is a *separate* register

**This is the single highest-impact trap in this section.** On Gen11/Gen12 the ultimate display
interrupt gate is **`DISPLAY_INT_CTL` at `0x44200`, bit 31 (`DISPLAY_IRQ_ENABLE`)** — *not* bit 31
of `DEISR`/`DEIER`.

`[I915]` `i915_reg.h:2612-2613`:

```
GEN11_DISPLAY_INT_CTL   = 0x44200
GEN11_DISPLAY_IRQ_ENABLE = 1 << 31
```

and `display/intel_display_irq.c:1253-1263` writes exactly that register (and nothing else) in
`gen11_de_irq_postinstall`. Cached RKL Volume 2 states it in prose: *"DISPLAY_INT_CTL, Address
44200h … bit 31 Display Interrupt Enable … This is the ultimate control for display interrupts. This
must be enabled for any of these interrupts to propagate."* **i915 never writes `DEIER` on Gen12.**

`[INF]` The name `DE_MASTER_IRQ_CONTROL` still exists in `[I915]` `i915_reg.h:2403` at bit 31 of the
legacy `DEISR` block, which is why it is easy to reach for — but it is not the Gen12 gate. An
earlier draft of this document made exactly that mistake.

Routing, corrected:

```
DISPLAY_INT_CTL (0x44200) bit 31 = DISPLAY_IRQ_ENABLE   <-- enable this first
        |
DEISR / DEIER (0x44000 / 0x4400C)  -- top-level, per-block routing
        |
        +-- GEN8_DE_MISC_IRQ  = 1<<22 --> GEN8_DE_MISC_IIR  (0x44468)
        +-- GEN8_DE_PORT_IRQ  = 1<<20 --> GEN8_DE_PORT_IIR  (0x44448)
        +-- GEN8_DE_PIPE_x_IRQ = 1<<(16+pipe) --> GEN8_DE_PIPE_IIR(pipe) (0x44408+pipe*0x10)
        |
SDEISR / SDEIER (0xC4000 / 0xC400C) -- south display (hotplug, GMBUS)
```

### 10.4 Per-pipe interrupts

| Bit | Name |
|---|---|
| 31 | `PIPE_FIFO_UNDERRUN_STATUS` |
| 26 | `PIPE_HOTPLUG_INTERRUPT_ENABLE` |
| 25 | `PIPE_VSYNC_INTERRUPT_ENABLE` |
| 17 | `PIPE_VBLANK_INTERRUPT_ENABLE` (and `PIPE_START_VBLANK_INTERRUPT_ENABLE` at 18) |
| 1 | `PIPE_VBLANK_INTERRUPT_STATUS` / `PIPE_FRAMESTART_INTERRUPT_STATUS` |
| 0 | `PIPE_HBLANK_INT_STATUS` |

`[I915]` `i915_reg.h:1626-1690`. The per-pipe `PIPESTAT(pipe)` register is at `0x70024 + pipe*0x1000`
and carries the status half in `[15:0]` and the enable half in `[30:16]`
(`PIPESTAT_INT_ENABLE_MASK = 0x7fff0000`, `PIPESTAT_INT_STATUS_MASK = 0x0000ffff`).

**To enable vblank on pipe A:** set `PIPE_VBLANK_INTERRUPT_ENABLE` in `PIPESTAT(A)`, and route
`GEN8_DE_PIPE_A_IRQ` up through `DEIER`.

**To confirm the pipe is running without interrupts at all:** poll `PIPEDSL(A)` (`0x70000`,
`[19:0]` = current scanline). It must change. This is the single most useful sanity check in the
whole document.

### 10.5 Hotplug interrupts

South display, `SDEISR`/`SDEIER` at `0xC4000`/`0xC400C`. `[I915]` `i915_reg.h:2996-3012`:

| Bit | Name |
|---|---|
| `SDE_DDI_HOTPLUG_ICP(hpd_pin)` = `1 << (16 + (hpd_pin - HPD_PORT_A))` | DDI A/B/C/D |
| `SDE_TC_HOTPLUG_ICP(hpd_pin)` = `1 << (24 + _HPD_PIN_TC(hpd_pin))` | TC1..TC6 |
| `SDE_GMBUS_ICP` = `1 << 23` | **GMBUS transaction complete** |
| `SDE_PICAINTERRUPT` = `1 << 31` | PICA |

So for DDI A: `SDE_DDI_HOTPLUG_ICP(HPD_PORT_A) = 1 << 16`; DDI B: `1 << 17`.

**A GMBUS completion interrupt exists** (`SDE_GMBUS_ICP`, bit 23) and `[I915]` uses it to avoid
polling (`has_gmbus_irq`). For a first bring-up, poll `GMBUS2` instead — the polling path is the
same code and has a bounded timeout (`wait_for_us(..., 2)` then `wait_for(..., 50)` ms in
`gmbus_wait`). `[I915]` `display/intel_gmbus.c:335-360`.

### 10.6 Enabling them: the ordering trap

`[PRM]` "Sequence to Enable a SCDC Interrupt" documents a general hazard that applies to *all*
display interrupts, in these words:

> *"Set interrupt IMR to masked (1) for this interrupt. This is needed to prevent a false interrupt
> as [the source] enables. Set interrupt IER to enabled (1). Clear interrupt IMR to unmasked (0)."*

i.e. **mask → enable → unmask**, never enable-then-mask. `[INF]` The same three-step ordering is
what i915's `gen8_de_irq_postinstall` does for the port interrupt block.

### 10.7 Clearing

Write 1 to the bit in the **IIR** register. Because second-level bits are shared, the standard
pattern is:

```
iir = read(DEIIR)                       /* snapshot: unmasked + pending */
if (iir & GEN8_DE_PORT_IRQ) {
    u32 port_iir = read(GEN8_DE_PORT_IIR);
    write(GEN8_DE_PORT_IIR, port_iir);  /* W1C */
    ...
}
write(DEIIR, iir);
```

And for south display: read `SDEIIR`, handle, write `SDEIIR` back.

`[INF]` from the standard Intel IIR semantics plus `[I915]`'s handler structure in
`display/intel_display_irq.c` and `display/intel_hotplug_irq.c`.

### 10.8 Underrun reporting

`PIPE_FIFO_UNDERRUN_STATUS` (bit 31 of the per-pipe ISR) is your early warning that the DDB or
watermarks are wrong. **Enable and log it from day one.** An underrun that you never see is an
underrun you will misattribute to timing or the monitor.

---

## 11. The minimal bring-up order

This is the checklist. It assumes: **pipe A, transcoder A, combo PHY A, HDMI/DVI, one plane,
linear XRGB8888, no scaling, no colour management, no audio, DC states disabled, no DMC.**

Each step says what to do, what to poll, and — more usefully — **what it looks like when it silently
did nothing.**

### Phase 0 — before touching the display

**0.1 Enumerate the PCI device.** Expect `8086:46d0`, one function, BAR 0 of 16 MiB.
*If BAR 0 is not 16 MiB:* you may be looking at the wrong function or the firmware left the BAR
unassigned. Fix that before continuing.

**0.2 Map BAR 0** uncached for the register window. Do not map BAR 2 yet.

**0.3 Read the "am I allowed to do this" registers and log them:**
```
SKL_DFSM      0x51000    -> pipe disable fuses, DMC/DSC/HDCP availability
SFUSE_STRAP   0xC2014    -> raw clock strap (bit 8), display-disabled (bit 7)
SKL_DSSM      0x51004    -> CDCLK PLL reference frequency, bits [31:29]
```
*If you skip this*, and the machine's pipe A is fused off, everything downstream fails with no
diagnostic.

**0.4 Read PCI config space BAR0/BAR2 and log the actual sizes.** Do not assume.

### Phase 1 — power

**1.1 Disable DC states:** `DC_STATE_EN (0x45504) = 0`.
*If not done:* the hardware may power-gate what you are programming, intermittently.

**1.2 Combo PHY init** for every combo PHY present, **PHY A first** (§8.3):
read `PORT_COMP_DW3` → pick procmon row → write `DW1` (masked), `DW9`, `DW10`; set `IREFGEN` on PHY A
only; set `DW0.COMP_INIT`; set `CL_DW5.CL_POWER_DOWN_ENABLE`; set `DW8` ODCC and `PCS_DW1` DCC first
(Gen12).
*Looks like it did nothing:* `COMP_INIT` does not stay set. Re-read it. If it reads back 0, the PHY
is not powered — you are missing `PW_1`.

**1.3 Enable `PW_1`** in `HSW_PWR_WELL_CTL2`:
```
apply Wa_16013190616: rmw(GEN8_CHICKEN_DCPR_1 0x46430, 0, DISABLE_FLR_SRC /*bit15*/)
poll SKL_FUSE_STATUS.PG0_DIST_STATUS
rmw(0x45404, 0, REQ(PW_1)/*0x2*/)
poll STATE(PW_1)/*0x1*/ == 1
poll SKL_FUSE_STATUS.PG1_DIST_STATUS
```
*Looks like it did nothing:* `STATE` never sets. Causes, in order of likelihood: `PW_1` index wrong
(it is 0, REQ bit `0x2`); the fuse bit is at `1 << (27 - 1) = 1<<26`; another requester is holding
the well with a *different* bit pattern — read all four `PWR_WELL_CTL[1-4]` registers and compare.

**1.4 Enable CDCLK** (§4.6). **First check whether you need to.** i915's
`bxt_cdclk_init_hw` calls `bxt_sanitize_cdclk` and then returns early if the PLL is already
enabled with a valid VCO — it does **not** reprogram a working CDCLK. `[I915]`
`display/intel_cdclk.c` `bxt_cdclk_init_hw`. Read `CDCLK_PLL_ENABLE` and `CDCLK_CTL` first; if the
firmware left you a usable CDCLK, keep it.

If you do need to change it, and there is no pipe running yet, the simple path is:
```
write(CDCLK_PLL_ENABLE 0x46070, RATIO)                 /* ratio for the ref freq from SKL_DSSM */
write(CDCLK_PLL_ENABLE, RATIO | PLL_ENABLE /*bit31*/)
poll LOCK /*bit30*/                                    /* spec timeout 200us */
write(CDCLK_CTL 0x46000, CD2X_DIV_SEL | CD2X_PIPE(A) | DECIMAL(cdclk))
```
Note there is **no** bit-27 power-up step here. That step belongs to the combo DPLL registers
`0x46010`/`0x46014` (§6.3), not to the CDCLK PLL.

*Looks like it did nothing:* `LOCK` never sets. Almost always the wrong `RATIO` for the wrong
reference frequency. **Re-read `SKL_DSSM`** — if you assumed 38.4 MHz and the hardware says
19.2 MHz, every ratio is wrong by 2×. `[TGL12]` says the same thing in the same order: *read
`DSSM` first, because it selects both the PLL ratio table and the PG1 enable timeout.* This is the
specific failure that the in-flight ADL-N libgfxinit work reports.

**1.5 Enable the DBUF slices** (§4.7): set `DBUF_POWER_REQUEST` on each present slice, poll
`DBUF_POWER_STATE`. Read the state first — if `DBUF_POWER_STATE` is already 1, the firmware left
them on.
*Looks like it did nothing:* the pipe runs but every frame underruns (`PIPE_FIFO_UNDERRUN_STATUS`).

**1.6 Apply the platform workarounds:** `GEN11_CHICKEN_DCPR_2 (0x46434)` per
`Wa_14011508470`; consider **leaving** `XELPD_DISPLAY_ERR_FATAL_MASK (0x4421c)` unmasked so errors
are visible.

### Phase 2 — the sink

**2.1 Enable the AUX/DDC power well** for the port (`ICL_PWR_WELL_CTL_AUX2 0x45444`, index 0 for
AUX_A) — needed for GMBUS/DCC on that pin pair.
*Looks like it did nothing:* GMBUS returns NAK on every address, always.

**2.2 Enable hotplug**, then immediately read the status once (per the PRM's advice):
```
SHOTPLUG_CTL_DDI (0xC4030): set HPD_ENABLE for the DDI
read SDEISR (0xC4000): SDE_DDI_HOTPLUG_ICP gives the live connect state
```
*Looks like it did nothing:* bit never sets. Check HPD polarity (`0xC2000[18:15]`, §9.5) and check
that you enabled the *right* DDI index.

**2.3 Read the EDID over GMBUS** (§9.3). 128 bytes from slave `0x50`, register `0x00`.
Try **pin index 1 (DDI A)** first, then **pin index 2 (DDI B)** — a monitor's EDID will appear on
exactly one of them and that identifies your physical port. Checks: header
`00 FF FF FF FF FF FF 00`; the EDID checksum (sum of all 128 bytes ≡ 0 mod 256); then parse the
detailed timing descriptor.
*Failure modes and their meaning:* see §11.1 below. **Do not proceed past this step until the EDID
is valid.** A wrong EDID gives you wrong timings and you will chase a display bug that is really a
parse bug.

### Phase 3 — the mode

**3.1 Choose the mode.** Prefer the EDID's **preferred timing** (first detailed descriptor), and
prefer 1920×1080@60 if it is offered — that is 148.5 MHz, comfortably inside the HBR table, and
needs no HDMI scrambling.
*Sanity check:* the pixel clock must be ≤ what your CDCLK can support, and the mode must fit the
plane.

**3.2 Allocate the framebuffer** in memory the display engine can read — which on an integrated GPU
means memory mapped through the **GGTT**. Write the GGTT PTE for it. Use a linear, contiguous
allocation; make the surface stride a multiple of 64 bytes (256 is safest) and the base address
4 KiB-aligned.

**3.3 Compute the PLL dividers** for the pixel clock (§6.3) and **log them** before writing.

**3.4 Program the timing registers** (§6.1) — remember **every field is `value − 1`**.
*Looks like it did nothing:* the monitor reports "no signal" or shows a rolling/off-centre image.
A rolling image means the timings are close but a total is off by one; a black screen with correct
sync means the plane or DDI is wrong.

### Phase 4 — the pipe

**4.1 Program the DDB** for the plane: `PLANE_BUF_CFG(A,1) = ((4096-1) << 16) | 0 = 0x0FFF0000`.

**4.2 Program the watermarks** (§7.3). Start with the generous version: level 0
`EN | BLOCKS(allocation) | LINES(31)`, all other levels disabled.
*Looks like it did nothing:* **this is the most common cause of a black screen with apparently
correct timings.** If `PLANE_WM_EN` is 0 the plane reads nothing.

**4.3 Program the "noarm" plane registers** — stride, position, size, `PLANE_OFFSET`, key
registers = 0, `PLANE_COLOR_CTL` = alpha disabled, `PLANE_CTL` = `ENABLE | FORMAT_XRGB8888 |
TILED_LINEAR`, and finally `PLANE_SURF = ggtt_address`.
*Looks like it did nothing:* `PLANE_SURFLIVE` reads back 0 — the plane never armed. Re-read
`PLANE_CTL`; if `ENABLE` is set but `SURFLIVE` is 0, the surface address was rejected (alignment or
a GGTT entry that is not valid).

### Phase 5 — the output

**5.1 Program the PLL** — dividers, then power, then enable, then poll `LOCK`. Four things to get
right, all in §6.3:
```
ref = SKL_DSSM[31:29] decoded          /* 24 / 19.2 / 38.4 MHz */
if ref == 38400: ref = 19200           /* icl_wrpll_ref_clock: DPLL auto-divides by 2 */
(P,Q,K) = search(port_clock*5, ref)    /* icl_calc_wrpll: midpoint 8999 MHz, DCO in [7998,10000] */
if ref_strap == 38400: dco_fraction /= 2   /* WA #22010492432, applies to ADL-P/N */
```
*Looks like it did nothing:* `LOCK` never sets, or the mode comes out at the wrong pixel clock
(monitor reports "out of range" or shows a doubled/halved image). The two clamps above are the
usual cause: using 38.4 MHz instead of 19.2 MHz in the arithmetic makes every divider wrong by 2×,
and skipping the fraction halving makes it wrong by a fraction of a percent — small enough to look
like a marginal-signal problem rather than an arithmetic bug. **Print `ref`, `(P,Q,K)` and the
resulting `symbol_rate` before writing**, and compare the symbol rate against the mode's pixel
clock.

**5.2 Map DDI → PLL**: `rmw(ICL_DPCLKA_CFGCR0 0x164280, DDI_CLK_SEL_MASK(phy), DDI_CLK_SEL(pll_id, phy))`
then, **in a separate write**, clear `DDI_CLK_OFF(phy)`.

**5.3 Program the transliterated buffer values** for the port type and swing level (§8.5), then
**power up the lanes**: `PORT_CL_DW10` `PWR_DOWN_LN_MASK`.

**5.4 `TRANS_CLK_SEL(A) = (PORT_A + 1) << 28` = `0x10000000`.**

**5.5 `TRANS_DDI_FUNC_CTL(A) = ENABLE | SELECT_PORT(A) | MODE_SELECT_HDMI | BPC_8 | PHSYNC? | PVSYNC?`**
— note `SELECT_PORT(A)` in Gen12 encoding is `(0+1) << 27 = 0x08000000`.

**5.6 `TRANSCONF(A) = ENABLE | progressive`** = `(1<<31)`.

> **Corrected: bit 30 is a status, not a request.** Earlier revisions of this
> document wrote `ENABLE | STATE_ENABLE = (1<<31) | (1<<30)`.  `[I915]` defines
> bit 30 as `TRANSCONF_STATE_ENABLE` (`i915_reg.h:1591`) and only ever *polls* it
> clear -- `intel_wait_for_pipe_off` (`display/intel_display.c:302-318`) -- while
> `intel_enable_transcoder` reads the register and writes back
> `val | TRANSCONF_ENABLE`, bit 31 alone (`:459`, `:474-475`).  Setting a status
> bit is writing a request that the hardware does not read; the bit can, however,
> be read in phase 6 as a further "the pipe is up" proof.
Output bit depth is **not** set here on Gen12 — put `PIPE_MISC_BPC_8` (`PIPE_MISC[7:5] = 0`) and
`PIPE_MISC_DITHER_ENABLE` (`PIPE_MISC[4]`) in `PIPE_MISC(A)` (`0x70030`) if you want dithering.

**5.7 `DDI_BUF_CTL(A) = ENABLE | BUF_TRANS_SELECT(level) | PHY_LINK_RATE(rate) | PORT_WIDTH(lanes-1)`**
then poll `IS_IDLE == 0`.
*Looks like it did nothing:* `IS_IDLE` stays 1. That means the DDI has no clock — check 5.2 and 5.1,
in that order. `IS_IDLE` is the single best "is my DDI alive" bit on the chip.

> **A known real-hardware failure at exactly this step.** `[I915]` bug #10932 reports an
> **N200 / `46d0`** machine failing with *"Timeout waiting for DDI BUF **D** to get active"* under
> coreboot + EDK2. It is the closest thing to a field report for this exact bring-up. If you hit a
> DDI-buffer idle poll that never clears, check the port/`aux_ch` mapping and which DDI is actually
> wired before suspecting your PLL — a wrong DDI is a far more common cause than a wrong divider.
> Related reports on neighbouring parts: #15690 (N100, Type-C PHY warning plus display loss on HDMI
> hotplug), #15924 (N150, DP link training after USB-C hotplug).
>
> Note also that **no ADL-N-specific display workaround exists in mainline i915.** `IS_ALDERLAKE_N`
> was proposed and rejected in review; the macro is `IS_ALDERLAKE_P_N()`, and every mainline commit
> mentioning ADL-N is enablement, PCI-ID, PCH, stepping or GuC plumbing. The one mechanism that
> genuinely changes ADL-N display behaviour is that its **display** stepping is `STEP_D0` while its
> GT stepping is A0 — so A0/B0-bounded ADL-P display workarounds are silently skipped.

### Phase 6 — prove it

**6.1 Read `PIPEDSL(A)` twice, a few milliseconds apart.** It must change. If it does not, the pipe
is not scanning and nothing downstream matters.

**6.2 Read `PLANE_SURFLIVE(A,1)`.** It must equal the address you wrote.

**6.3 Read `DDI_BUF_CTL(A).IS_IDLE`.** Must be 0.

**6.4 Read `PIPESTAT(A)` bit 31 (FIFO underrun).** If set, your watermarks or DDB are wrong — go
back to 4.2 before changing anything else.

**6.5 Fill the framebuffer with a known pattern** — vertical colour bars, not solid black. A solid
black framebuffer is indistinguishable from a black screen caused by every other failure in this
list. **This is the single highest-value debugging decision in the whole document.**

**6.6 If the monitor shows the pattern**, congratulations: you have a linear framebuffer being
scanned out. Harden it (correct watermarks, real EDID modes, hotplug, interrupts) afterwards.

### 11.1 GMBUS failure modes and recovery

| Symptom | Meaning | Recovery |
|---|---|---|
| `GMBUS2.SATOER` set | Sink NAKed, or no device at that address | Right pin? Right address (`0x50` for DDC)? Monitor on? |
| `GMBUS2.STALL_TIMEOUT` set | A secondary held the clock too long | Bus is stuck; reset (below) |
| `GMBUS2.ACTIVE` never clears | Transaction never terminated | Reset (below) |
| GMBUS returns `0xFF` bytes | Bus floats high, no device | Wrong pin, or no pull-ups (`[PRM]` warns GMBUS must not be initiated without pull-ups) |
| EDID header reads but checksum fails | Partial/stale read | Re-read; if persistent, lower the rate to 100 kHz or 50 kHz |
| Works once, then never again | Bus left in a bad state | Reset (below) |

**Recovery / bus reset** — `[I915]` `intel_gmbus_reset` (`display/intel_gmbus.c:209-214`):
```
write(GMBUS0, 0)
write(GMBUS4, 0)
```
Then wait one full EDID transaction time and retry. `[I915]` calls this on every GMBUS setup
(`intel_gmbus.c:314`) and on shutdown (`:933`).

If a plain reset does not clear it, the remaining tool is **bit-banging** the GPIO pair: `[PRM]`
"GPIO Programming for I2C Bit Bashing" — *"To drive GPIO pin low, program direction to 'out' and
data value to '0'. To drive GPIO pin high (tristate to allow external pull up to activate), program
direction to 'in', along with mask bit."* Nine clock pulses with the data line released is the
standard I2C bus-recovery dance.

---

## 12. What to read on the actual machine

The target is real hardware, so these reads replace every guess in this document. Do them in this
order and **log the raw values** — a from-scratch driver's most valuable early artefact is a
register dump from a machine where the vendor driver worked, compared with one where it did not.

### 12.1 Identity and configuration

| Read | Address | Settles |
|---|---|---|
| `SKL_DFSM` | `0x51000` | Which pipes are fused off. Which of DMC/DSC/HDCP/FBC exist. |
| `SKL_DSSM[31:29]` | `0x51004` | **The CDCLK PLL reference frequency: `000b` = 24 MHz, `001b` = 19.2 MHz, `010b` = 38.4 MHz.** `[TGL2C]` and `[I915]` `i915_reg.h:2880-2884` agree exactly. This selects **both** the PLL ratio table and the PG1 enable timeout. **Read it first.** |
| `SKL_DSSM[6]` | `0x51004` | `DE_8k_DIS` |
| `SFUSE_STRAP` | `0xC2014` | Raw clock strap (bit 8); display-disabled (bit 7); legacy DDI-detect bits. |
| `FUSE_STRAP` | `0x42014` | Additional strapping. |
| `FUSE_STATUS` | `0x42000` | Per-power-gate distribution status (bit 31 = fuse download status; `[TGL12]` gives PG0..PG5 at bits 27/26/25/24/23/22). |
| `DC_STATE_EN` | `0x45504` | `[1:0]` dynamic DC state (00 = disabled, 01 = up to DC5, 10 = up to DC6); bit 3 = DC9 allow; bit 31 = mode-set-in-progress; bit 30 = display clock off enable. **Bits 9/8/4 are hardware-communication only — software must not change them.** `[TGL2C]`+`[I915]`. |
| PCI config `0x48` | — | `MCHBAR` base, for DRAM geometry. |
| PCI config BAR0/2 | — | Actual BAR sizes — do not assume 16 MiB. |

### 12.2 Which PHYs and ports exist

| Read | Address | Settles |
|---|---|---|
| `PORT_COMP_DW0` for PHY A, B | `0x162100`, `0x06C100` | `COMP_INIT` — does the PHY exist and is it initialised? A read of `0xFFFFFFFF` or `0x00000000` on **both** the read and a re-read means **the PHY instance is absent**. |
| `PORT_COMP_DW3` | `+0x10C` | Silicon process/voltage variant → selects the procmon row. |
| `ICL_PHY_MISC` | `0x64C00`, `0x64C04` | Which combo PHYs have a `PHY_MISC` instance. |
| `SHOTPLUG_CTL_DDI` | `0xC4030` | After enabling HPD, which DDIs report a live connection. **This is the authority on which ports are wired on this board.** |
| `GMBUS0` pin sweep | `0xC5100` | Try pin indices **1** and **2** (DDI A and B, 1-based); the one that returns a valid EDID identifies your physical port. |

`[INF]` The "does this PHY exist" test deserves care: on many Intel parts an absent block reads as
all-zeros or all-ones, but a *present* block in reset can also read zero. Probe with a
write/read-back of a scratch field (e.g. write a known pattern to an unused `PORT_COMP_DW1` bit and
read it back) before concluding absence.

### 12.3 The reference clock and timing

| Read | Settles |
|---|---|
| `SKL_DSSM[31:29]` | The CDCLK reference (§12.1). |
| `SFUSE_STRAP[8]` | The **raw clock** strap: 24 MHz vs 19.2 MHz. |
| `PCH_RAWCLK_FREQ` (`0xC6204`) | What the firmware programmed. **If a vendor driver or the firmware has already run, this is the ground truth** — read it before you overwrite it. |
| `BXT_DE_PLL_ENABLE` (`0x46070`) | Ratio and lock state left by firmware. |
| `CDCLK_CTL` (`0x46000`) | CD2X divider and decimal left by firmware. |
| `PIPEDSL(A)` over time | **Empirical proof the pipe is running and at what rate.** Sample it at a known interval; the delta gives the line rate, from which you can *derive* the true pixel clock. **This is the only way to verify your PLL arithmetic against reality without a scope.** |

### 12.4 Is there a panel?

| Read | Settles |
|---|---|
| `PP_STATUS` (`0x61200`) | `PP_ON` and the sequence state. `PP_SEQUENCE_STATE_ON_IDLE` = a panel is on. |
| `PP_CONTROL` (`0x61204`) | `EDP_BLC_ENABLE`, `PANEL_POWER_ON`. |
| `SFUSE_STRAP[7]` | Display-disabled strap. |
| GMBUS pin 1 / DDC on DDI B | Many laptop panels hang off DDI B; if an EDID appears there with a laptop-shaped descriptor, you have a panel. |
| GMBUS pin 2 / `GMBUS_PIN_3_BXT` | The eDP AUX/DDC mapping on some designs. |

`[INF]` On a mini-PC with only HDMI/DP outputs, expect `PP_STATUS` to be idle and no EDID on the
low pins. That is a valid, useful negative result: it tells you to skip §8.7 entirely.

### 12.5 What the sink actually supports

Read the EDID: bytes `0x00`–`0x7F` from slave `0x50` register `0x00`. Then, for HDMI sinks, read the
**CEA-861 extension** (block 1: write register `0x00` = `0x01` to slave `0x50` with the "current
address" protocol, then read 128 bytes). The extension tells you the video modes the sink accepts
and whether it is an HDMI sink at all (vs DVI) — which decides
`TRANS_DDI_MODE_SELECT_HDMI` vs `_DVI`.

**Compare the EDID's preferred timing to what the firmware left in `HTOTAL`/`VTOTAL`.** If they
agree, you have a fully independent confirmation of both your EDID parser and your timing
understanding. **This is the highest-value single check available on the real machine.**

---

## 13. Gaps and uncertainties

Explicitly listed. Every one of these is a place where I would rather you read a register than
trust this document.

### 13.1 Things I could not source at all — `[GAP]`

1. **No Alder Lake / ADL-N PRM exists.** Confirmed by enumerating Intel's own PRM platform list:
   12th-generation client is absent; the Gen12 entries stop at Tiger Lake, Rocket Lake, DG1,
   Lakefield, Alchemist/ATS-M and ICL. Everything register-level in this document is
   TGL- or DG1-sourced and then corrected for the Gen13 deltas that i915 exposes. **Given that TGL
   and RKL — two Gen12 projects — have different power-well maps, the ADL-N map must be verified on
   hardware, not inferred.**
2. **`GMBUS1`–`GMBUS4` are absent from every public Gen12 register volume.** The TGL 2c (parts 1
   and 2), DG1 2c (parts 1 and 2) and RKL Vol 2 PDFs were grepped; only `GMBUS0` is defined (DG1
   mentions "GMBUS4 Interrupt Mask" once, without a definition). The transaction protocol in §9.3
   therefore has a **single source: `[I915]`.** Recorded as a confirmed hole, not a failure to
   search.
3. **Which pipes/ports/PHYs a given i3-N305 SKU actually has.** Requires the DSFM read (§12.1) and
   the hotplug read (§12.2).
4. **The ADL-N CDCLK PLL reference frequency.** Contradictory between the DG1 PRM (38.4 MHz, "not
   programmable") and TGL/i915 (read from `SKL_DSSM`). Resolve by reading `SKL_DSSM`.
5. **The ADL-N raw clock frequency.** Same shape of disagreement (38.4 vs 24/19.2). Resolve by
   reading `SFUSE_STRAP[8]` and `PCH_RAWCLK_FREQ`.
6. **The ADL-N CDCLK voltage-level table.** `tgl_calc_voltage_level` is Gen12-specific and I did
   not verify it against a PRM.
7. **The `PDIV`/`KDIV` field encoding.** **CLOSED — it was a false alarm of this document's own
   making.** §6.3's "The `PDIV`/`KDIV` encoding — resolved" records what the apparent conflict
   actually was: the *Skylake* encoder `skl_wrpll_params_populate` compared against the *Gen12*
   decoder, which are not on the same path. ADL-N runs `icl_wrpll_params_populate`, which emits
   exactly the Gen12 named-constant values, so write and read round-trip and there is no
   discrepancy to settle. What remains live is the trap §6.3 describes — both encoders write one
   `struct skl_wrpll_params` — and what remains *unverified* is only whether the silicon implements
   the named-constant encoding, which §13.4's read-back settles rather than any document.
8. **Where ADL-P/N programs `DBUF_TRACKER_STATE_SERVICE`.** `gen12_dbuf_slices_config` explicitly
   returns early for ADL-P. Either the reset value is correct or the programming is elsewhere.
9. **Whether ADL-N needs the 16 Gb-DIMM level-0 latency adjustment** for its soldered LPDDR5.
10. **What register `0xC2000` is**, which the DG1 PRM says must have bits `[18:15] = 1111b` for
    hotplug board inversion.
11. ~~**The exact `skl_ddi_calculate_wrpll` search loop.**~~ **CLOSED.** The ADL-N search loop is
    `icl_calc_wrpll`, not `skl_ddi_calculate_wrpll`, and it is fully transcribed in §6.3 — bounds,
    divider list, selection rule, and the `icl_wrpll_get_multipliers` decomposition. The PRM's three
    DCO constants match i915's three constants exactly. What remains unverified is only whether the
    arithmetic produces the *right* frequency on real silicon; the algorithm itself is sourced.
12. **The values of `icl_combo_phy_trans_hdmi`** — the HDMI buffer-translation table for combo PHY.
    I identified which table is selected but did not extract its entries.
13. **The ADL-N VBT / OpRegion structure.** Out of scope by choice; needed only for eDP.
14. **DKL (Type-C) PHY programming is Bspec-only by Intel's own statement.** `[TGL12]` has a
    dedicated DKL section; the DG1 PRM explicitly defers to non-public documentation. Deferred here
    (§8.8) — but if you ever need it, the access mechanism is: a 4 KiB aperture per port PHY whose
    upper address bits come from `HIP_INDEX_REG0 = 0x1010A0` (apertures `0x168000`–`0x16B000`) or
    `HIP_INDEX_REG1 = 0x1010A4` (`0x16C000`–`0x16F000`); write the index, then the register.
    `[TGL12]` warns that addresses printed inside PHY register descriptions **omit the index** and
    must be ignored.
15. **No public Xe-LP hardware ISA exists.** Public material is Intel's IGC vISA appendix (MIT,
    lists TGLLP as a Gen12.1 platform column) plus compute-level optimisation guides, none of which
    has display content. Not needed for this document, but recorded so nobody goes looking.

### 13.2 Things that are inference — `[INF]`

- That the display and render blocks share one PCI function on ADL-N. Now **independently
  corroborated**: i915 binds only PCI function 0, coreboot's `SA_DEVFN_IGD` is `PCI_DEVFN(2,0)`, and
  Intel's 12th-gen datasheet documents all graphics BARs under device 0:2:0 — but I still found no
  single sentence that states it, so it stays listed here.
- That `GEN8_DE_PIPE_D_IRQ` is `1 << 19`. (The source macro `1 << (16 + pipe)` with `PIPE_D = 3`
  makes this near-certain, but the constant is not written out anywhere.)
- That the "generous level 0 watermark" approach in §7.3 produces a working display. It is a
  reasonable engineering position, not a sourced one.
- That the PRM's `0xC2000[18:15]` hotplug-inversion note is DG1-specific and does not apply to
  ADL-N boards.
- That the `SFUSE_STRAP` DDI-detect bits are advisory on Gen12. (`[I915]`'s `port_strap_detected()`
  returns true unconditionally for `DISPLAY_VER >= 9` and port presence comes from the VBT, which
  supports this — but the bits are documented, so they are not simply dead.)
- That the ADL-N GPIO pin table is exactly `gmbus_pins_icp`. `[I915]` selects it via
  `INTEL_PCH_TYPE >= PCH_ICP`; ADL-N's PCH type is reported as `PCH_ADP` (LPC/eSPI ID `0x5480`),
  which satisfies the condition — so this is now well-supported, but I did not read it out of a
  document that says "ADL-N uses the ICP pin table".
- That the ADL-N CDCLK voltage-level table is `tgl_calc_voltage_level`.

### 13.3 Where sources disagree, and which I trust

| Question | `[PRM]` / `[TGL12]` | `[I915]` (Gen12 integrated) | Trusted | Why |
|---|---|---|---|---|
| CDCLK PLL reference | DG1: 38.4 MHz fixed, "not programmable". TGL: read from `DSSM`, 24 / 19.2 / 38.4 | read from `SKL_DSSM`; same three values, same encoding | **both agree** | The TGL PRM and i915 agree bit-for-bit on `DSSM[31:29]`. This is now double-sourced. The DG1 statement is the DG1-specific special case. |
| Raw clock | DG1: 38.4 MHz. (TGL: not extracted) | 24 or 19.2 MHz from `SFUSE_STRAP[8]` | **i915** | i915's code path exists precisely because the value varies. |
| DDI voltage swing values | DG1 table | ADL-P table | **i915** | These are board-tuned; the platform-specific table is right. |
| CDCLK ratio table | TGL gives the 24 MHz column (`15/16/26/54/46/54` → 180/192/312/324/552/648) and the 38.4 MHz column; DG1 only 38.4 | ADL-P table adds 176.000/22 and drops 180/15 and 324/54 | **i915** | i915's `adlp_cdclk_table` is the platform-specific one and is a superset. |
| `CDCLK_FREQ_DECIMAL` | U10.1 of `round_to_0.5MHz(f) − 1` | `DIV_ROUND_CLOSEST(f_kHz − 1000, 500)` | **both agree** | Verified numerically on two frequencies (§4.6). |
| `CD2X_PIPE` | `000/010/100/110/111` = A/B/C/D/none | `pipe << 20`, none = `7 << 19` | **both agree** | Bit-for-bit. |
| `DPLL_CFGCR0/1` layout | `CFGCR0`: frac `[24:10]`, int `[9:0]`; `CFGCR1`: qdiv `[17:10]`, mode `[9]` | identical | **both agree** | Bit-for-bit. This *replaced* an earlier draft that used the Skylake `CFGCR2` layout — that was wrong. |
| `PDIV`/`KDIV` encoding | `P ∈ {2,3,5,7}`, `K ∈ {1,2,3}`, `K≠2 ⇒ Q=1` | **agrees on the ADL-N path**: `icl_wrpll_get_multipliers` + `icl_wrpll_params_populate` satisfy the PRM rules by construction, and write/read round-trip | **resolved** | An earlier draft claimed an unresolved conflict; it had compared the *Skylake* populate function against the Gen12 decoder. The real finding is a two-conventions-one-struct trap, not a live inconsistency. See §6.3. |
| Power-well numbering | TGL/DG1: `PG0..PG5` chain, bits 1/0…9/8. RKL: different again | `XE_LPD`: tree, `PW_1`=0, `PW_2`=1, `PW_A..D`=5..8 | **i915 for ADL-N** | i915's `XE_LPD` map is the only source that targets display 13. TGL and RKL disagreeing with *each other* is the warning. |
| Hotplug board inversion | DG1 says apply `0xC2000[18:15]=0xF` | does not program it | **unresolved** | Board-specific. Try both polarities. |
| Combo PHY DCC mode | TGL says "DCC continuous mode"; DG1 says "divide by 2" | programs `RUN_DCC_ONCE` in `PCS_DW1` for Gen12 | **i915** | Two PRM volumes contradict each other on the same step; i915 is unambiguous and targets Gen12. |

### 13.4 The one thing I would do first on real hardware

Boot the machine with a vendor driver (or just the firmware's own GOP), **let it modeset
successfully, then dump the display register window before anything clears it.** A 2 MiB dump from a
*working* configuration is worth more than every table in this document, because it resolves
§13.1 items 1, 3, 4, 5, 6, 7, 9 and 12 in a single shot, and it gives you a known-good target to
diff against.

Dump at minimum: `0x00000`–`0x7FFFF` (pipes, transcoders, planes, power wells, CDCLK, DDI) and
`0xC0000`–`0xCFFFF` (south display). Then diff two dumps: one with a monitor attached and one
without. **The difference is the minimum set of registers that matter.**

Also read, before overwriting anything, the registers i915 would refuse to reprogram:
`CDCLK_PLL_ENABLE` + `CDCLK_CTL` (i915's `bxt_sanitize_cdclk` keeps a working CDCLK rather than
replacing it), `DPLL0_CFGCR0`/`DPLL0_CFGCR1` (reuse the divider set verbatim — see the `PDIV`
encoding problem in §6.3), and `PCH_RAWCLK_FREQ`.

### 13.5 Tools worth having on the machine

- **`intel_reg` from igt-gpu-tools works, but only with an explicit spec file.** Its lookup order is
  `registers/<devid>` → `registers/<codename>` → `registers/gen<N>`; there is no `alderlake_n`,
  no `46d0` and no `gen12` entry, so it **silently falls back to a builtin Gen2–Gen7.5 spec** and
  will mis-name or refuse your registers. Pass the ADL-P file explicitly:
  ```
  intel_reg --spec=<igt>/tools/registers/alderlake_p ...
  ```
  (or set `INTEL_REG_SPEC`). That spec pulls `adlp_base.txt` (~2,100 named registers) plus a delta
  file, and is correct for ADL-N because the kernel models ADL-N as a subplatform of ADL-P.
- **The highest-value oracle workflow:** `intel_reg snapshot > mmio.bin` on a known-good i915 boot,
  then decode offline with `--mmio=mmio.bin --devid=0x46d0 --spec=…`, and **diff snapshots taken in
  different states**. The register delta *is* the modeset sequence, sourced from your own hardware.
- Other useful igt tools: `intel_watermark` and `intel_display_bandwidth` (both handle display
  version 13 explicitly), `intel_display_poller` (tells you *when* a write took effect),
  `intel_display_crc` (pipe CRC on pipes A–C), `intel_vbt_decode`, `intel_opregion_decode`,
  `intel_firmware_decode`, `lsgpu -c`.
- Two corrections to common assumptions: `modetest` does **not** dump EDID and belongs to libdrm,
  not igt; `intel_gpu_top` is a PMU engine-counter tool, not a display oracle. `edid-decode` now
  ships in `v4l-utils`.

---

## 14. Sources

All retrieved during this session. Facts were extracted; no source's code, comments or prose was
transcribed.

### 14.1 Primary — Intel documentation

- **Intel® Open Source Programmer's Reference Manual, Tiger Lake, Volume 12: Display Engine.**
  Doc Ref `IHD-OS-TGL-Vol 12-12.21`, Rev 1.0, December 2021. 6,130,450 bytes.
  Retrieved live from Intel's own CDR host:
  <https://cdrdv2-public.intel.com/705833/intel-gfx-prm-osrc-tgl-vol-12-display-engine.pdf>.
  **This is the closest public register-level documentation to Alder Lake-N** — same Xe-LP family,
  integrated part, same power-well scheme. Full text extracted cleanly with `pdftotext -layout`.
  *Sections used:* Power Wells (map, enable/disable, timeouts, "writes dropped when down"); Clocks
  (CDCLK, `CDCLK_CTL`/`CDCLK_PLL_ENABLE` bitfields, PLL ratio tables for both 19.2 and 38.4 MHz
  references, combo PHY PLL search bounds); Combo PHY (procmon, init/un-init, comp sources); DDI
  (buffer control, voltage-swing tables); GMBUS and GPIO; DC states; Type-C/DKL PHY access
  (`HIP_INDEX_REG0/1`).
- **Intel Open Source PRM, Tiger Lake, Volume 2c: Command Reference — Registers, parts 1 and 2.**
  <https://cdrdv2-public.intel.com/703046/intel-gfx-prm-osrc-tgl-vol-02-c-command-reference-registers-part-1.pdf>
  and <https://cdrdv2-public.intel.com/703047/intel-gfx-prm-osrc-tgl-vol-02-c-command-reference-registers-part-2.pdf>.
  **Vol 12 describes registers by name only; the MMIO offsets and bitfields are in Vol 2c. Both are
  needed.** This is where the `DPLL_CFGCR0/1` field positions and the `DSSM`/`DC_STATE_EN`
  definitions were confirmed.
- **Intel® Iris® Xe MAX Graphics Open Source Programmer's Reference Manual, For the 2020 Discrete
  GPU formerly named "DG1", Volume 12: Display Engine.** February 2021, Revision 1.0.
  Doc Ref `IHD-OS-DG1-Vol 12-2.21`. 366 pages. Retrieved from
  <https://xorg.freedesktop.org/docs/intel/DG1/intel-gfx-prm-osrc-dg1-vol12-displayengine.pdf>
  (`www.x.org` 301-redirects to `xorg.freedesktop.org`).
  *Sections used:* Display Overview; Mode Set (Initialize / Un-initialize / DisplayPort / HDMI-DVI /
  WD sequences); Clocks; Central Power (SAGV); Panel Power and Backlight; GMBUS and GPIO;
  Interrupts and Hot Plug; Display Watermark Programming; Combo PHY DDI Buffer.
  **Used as the independent cross-check.** It agreed with the TGL PRM on every shared offset and
  table tested, including the procmon values (exact), the `DSSM` reference-frequency encoding, and
  the watermarks algorithm.
- **Intel Open Source PRM index**, <https://www.x.org/docs/intel/> — used to locate the DG1 volumes.
  Also hosts ICL, LKF, RKL, SKL, BDW, HSW, IVB, SNB. **No Alder Lake volume exists** (confirmed by
  enumerating Intel's own PRM platform list).
- **Rocket Lake PRM Volumes 1–3**, <https://xorg.freedesktop.org/docs/intel/RKL/>. Used only as a
  third Gen12 data point: RKL's `vol02-commandreference-registers` carries display registers but no
  programming sequences, and its power-well map differs from TGL's — which is the evidence that
  these maps are per-project.

**Citation hygiene note.** Intel's combined-PDF link
(`cdrdv2-public.intel.com/getContent/772631`) is dead (301 → Intel 404) although Intel's docs pages
still link it. Use the `cdrdv2-public.intel.com/<id>/<file>.pdf` form above. Also: the HTML labels
on Intel's hardware-specs page are **off by one** relative to the CDR content IDs — trust the
filename in the redirect, not the label. CDR IDs: TGL vol01=703034, 2a=703042, 2b=703044,
2c-1=703046, 2c-2=703047, 2d=703050, vol03=703052, 04=703057, 05=703058, 06=703060, 07=703061,
08=703062, 09=703063, 10=705824, 11=705826, **12=705833**, 13=705835, 14=705836.

### 14.2 Primary — Linux `drm/i915`, v6.12

Tag `v6.12`, commit `adc218676eef25575469234709c2d87185ca223a`, retrieved as raw files from
`raw.githubusercontent.com/torvalds/linux/<sha>/drivers/gpu/drm/i915/…`.
**GPL-2.0. Used as a source of facts only — register offsets, bitfield layouts, sequences, timeouts,
magic constants and tables. No code, comment or prose was copied.**

Register headers (offsets and bitfields):
`i915_reg.h`, `i915_reg_defs.h`, `intel_pci_config.h`,
`display/intel_display_reg_defs.h`, `display/skl_universal_plane_regs.h`,
`display/skl_watermark_regs.h`, `display/intel_combo_phy_regs.h`,
`display/intel_dp_aux_regs.h`, `display/intel_gmbus_regs.h`, `display/intel_pps_regs.h`,
`display/intel_display_limits.h`, `display/intel_dmc_regs.h`, `display/intel_ddi_buf_trans.h`.

Device and platform data:
`display/intel_display_device.c` / `.h` (the `xe_lpd_display` descriptor, the ADL-N ID list and
stepping table, pipe/transcoder/cursor offsets, DFSM fuse handling),
`intel_device_info.c`, `i915_pci.c`, `include/drm/intel/pciids.h`.

Behaviour:
`intel_uncore.c` / `.h` (forcewake ranges, MMIO mapping, wait-for-register semantics),
`gt/intel_ggtt.c` (BAR sizes, GTT aperture offset),
`display/intel_display_power.c` (display core init/uninit, DBUF),
`display/intel_display_power_well.c` (power well handshake, timeouts, workarounds),
`display/intel_display_power_map.c` (the `XE_LPD` well tree and per-well domains),
`display/intel_cdclk.c` (reference clock readout, ratio tables, PLL sequences, raw clock),
`display/intel_dpll_mgr.c` (PLL ids, combo PLL arithmetic and enable sequence, ADL-P PLL set),
`display/intel_ddi.c` (DDI clock select, `TRANS_DDI_FUNC_CTL` construction, pre-enable sequences),
`display/intel_ddi_buf_trans.c` (buffer-translation tables and selection),
`display/intel_combo_phy.c` (combo PHY init, procmon tables, comp source rules),
`display/skl_universal_plane.c` (plane commit order, format/tiling/stride/DDB encoding),
`display/skl_watermark.c` (DBUF slices, allowed DBUF configurations),
`display/intel_gmbus.c` (GMBUS protocol, pin tables, reset),
`display/intel_pps.c` (PPS base selection),
`display/intel_dram.c` (DRAM geometry / memory latency),
`intel_pcode.c` (mailbox protocol).

### 14.3 Cross-check — independent Gen12 implementations

- **coreboot `libgfxinit`** (AdaCore), <https://github.com/coreboot/libgfxinit>. MIT-licensed by
  recollection — **verify before relying on it**. Its `Generation` type enum is
  `I945, G45, Ironlake, Haswell, Broxton, Skylake, Tigerlake, AlderlakeP`, so this is a genuine
  independent Gen12/Xe-LP display implementation; `common/tigerlake/` contains
  `hw-gfx-gma-combo_phy`, `hw-gfx-gma-connectors-tc`, `hw-gfx-gma-plls-combo_phy`,
  `hw-gfx-gma-plls-dekel_phy`, `hw-gfx-gma-power_and_clocks`, and `xelpd/` power-domain code.

  **It drives Alder Lake-N in coreboot mainline today.** Evidence: coreboot's
  `src/soc/intel/common/block/graphics/early_graphics.c` calls `gma_gfxinit()`; Google's **nissa**
  platform (Alder Lake-N) selects `MAINBOARD_HAS_EARLY_LIBGFXINIT`; nissa's
  `gma-mainboard.ads` lists `Port_List = (eDP, HDMI1, HDMI2, others => Disabled)`; and the Gerrit
  changes adding nissa/brox early graphics are **merged**. This is direct, independent confirmation
  that ADL-N display bring-up from scratch is achievable and that the port topology assumed in this
  document (external HDMI on combo PHYs) is real.

  Two cautions when using it as an oracle:
  - `gma_gfxinit(int *lightup_ok)` reports failure **only** for "no display found". A DP
    link-training failure is **not** propagated. A success return does not mean the link trained.
  - coreboot's own prose doc, `Documentation/gfx/libgfxinit.md`, lists what is "verified to work
    within coreboot" and **never mentions Gen12**. The documentation is stale relative to the code;
    do not let it convince you Gen12 is unsupported, but also treat "a generation constant exists"
    as weaker evidence than "a board ships it".

  A dedicated bare-`Alderlake` generation (a refactor on top of the working support) is still
  unmerged, and the ADL-N-specific work reports three concrete failure modes a from-scratch
  implementer will hit: the IoT FSP leaves the **dedicated CDCLK PLL disabled** via
  `CDCLK_PLL_ENABLE`, the **per-pipe power wells** need explicit handling, and **plane DBUF
  allocation** must be programmed. All three are covered in §4 and §7 of this document.

- **coreboot ADL-N VBT references** worth mining if a panel is involved:
  `src/mainboard/starlabs/starbook/adl_n` (several VBT revisions: "Update VBT to fix HDMI output",
  "fix panel timings", "raise panel PWM frequency") and `src/mainboard/lattepanda/mu`
  ("Make VBT compatible with ADL-N FSP IPU25.3").
  - libgfxinit names this part explicitly: `common/hw-gfx-gma-config.ads.template` contains
    `function Is_Alder_Lake_N (Device_Id : Word16) return Boolean is
    (Device_Id = 16#46d0# or 16#46d1# or 16#46d2#);`. That is direct, independent corroboration of
    the device-ID grouping in §1.1.
- **Haiku `intel_extreme`** — an **independent** (not ported) Intel driver, MIT-licensed by
  recollection. Its `Generation()` returns 12 for Tiger Lake / Alder Lake and it added ADL support
  in commit `0db74e1a` (2025-01-16, described as tested and confirmed working). Its
  `TigerLakePLL.cpp` cites `IHD-OS-TGL-Vol 12-12.21` by page. **Gap:** it carries `0x46D1`, not
  `0x46D0` — likely a one-line addition, but untested.
- **Redox OS `ihdgd`** — an independent Gen12 modeset driver in Rust (merged 2025-12-18), split into
  `gmbus` / `aux` / `ddi` / `dpll` / `pipe` / `transcoder` / `power` / `gpio`. Structurally the
  closest small-scale decomposition of Gen12 modeset available, and therefore a useful template for
  how to *organise* a from-scratch driver.
- **Fuchsia `intel-display`** — independent, targets Gen12. Its README mandates that code comments
  cite the PRM document reference, section title, part and page. That is a good process model even
  if you never read its code. (Its PRM hyperlinks point at the dead 01.org, so follow the
  `cdrdv2-public.intel.com` IDs in §14.1 instead.)
- **The BSDs are ports, not independent implementations**, and their Linux baseline varies enough to
  matter: OpenBSD is on Linux 6.18.x, FreeBSD tracks a recent LTS, and **NetBSD is on Linux
  5.6-rc3 and stops at Tiger Lake with no Alder Lake support at all** — a clean demonstration that
  "it's a port of i915" does not imply "it supports your part".

### 14.4 Checked and found not to help

- **Mesa `src/intel/`** — contains `blorp, ci, common, compiler, decoder, dev, ds, executor,
  genxml, isl, mda, metrics_library, nullhw-layer, perf, shaders, tools, vulkan, vulkan_haskv`.
  **There is no display or modeset directory.** `genxml/gen120.xml` was grepped for
  `TRANS_DDI_FUNC_CTL`, `DSPCNTR`, `PIPEACONF`, `DPLL`, `CDCLK`, `PLANE_CTL`: **zero hits.** Mesa's
  Gen12 XML is render/compute-only. Mesa is therefore **not** a display reference; it is useful only
  as a *client* of a kernel modeset driver, and for `src/intel/dev/` platform tables.
  Mesa is MIT (`docs/license.rst`: *"The core Mesa library is licensed according to the terms of the
  MIT license"*), but that is moot given the above.
- **Rocket Lake PRM `vol02-commandreference-registers`** — 388 pages, contains display registers
  (`PIPE_MISC`, `DISPLAY_INT_CTL`, `PIPE_DMSCANLINECOMP`, …) but is a flat register reference with
  no mode-set or power sequences. Useful as a second opinion on a specific bitfield; not a
  substitute for DG1 vol12.

---

## 15. Colophon

- **Document written from source.** Every offset, bitfield, table and timeout in this document was
  read out of a file or PDF retrieved during the session that produced it. Nothing is from memory.
  Where a claim is reasoning rather than reading, it is tagged `[INF]`; where a source was missing,
  it is tagged `[GAP]`.
- **No hardware was run.** No part of this has been validated on real silicon.
- **An adversarial fact-check pass was run against the cached sources before this document was
  first committed**, and it found real errors, all of which are fixed and left visible in the text
  because each is a trap worth knowing about:
  1. The CDCLK PLL sequence wrongly carried the combo DPLL's bit-27 power-up step.
  2. The `DPLL_CFGCR1` field positions were the Skylake `CFGCR2` layout, not the Gen12 one.
  3. The GMBUS pin index was treated as 0-based when it is 1-based.
  4. The global display interrupt enable was given as `DEISR[31]` instead of `DISPLAY_INT_CTL`
     (`0x44200[31]`) — **the highest-impact of the four**, since a driver following it would enable
     nothing.
  5. Four rows of the `TRANS_DDI_FUNC_CTL` bitfield table were wrong (mode-select values, polarity
     bits, and both HDMI high-rate bits).
  6. The 19.2 MHz `PCH_RAWCLK_FREQ` value was mis-added (`0x10040800` → `0x10130800`).
  7. Two internal contradictions (the forcewake range vs. the combo PHY's `0x162000`; the
     `has_fuses` sentence for `PW_A`…`PW_D`).
  8. Bit depth and dithering were attributed to `TRANSCONF` when on Gen12 they live in `PIPE_MISC`.
  9. The `dco_fraction` formula had a dimension error (`ref_kHz` where `ref_MHz` belongs).

- **A second review round, prompted by two independent implementation workstreams, found four more
  defects.** All are fixed:
  10. **§8.2 gave the `PORT_TX_DW*` group base as `+0x400`; it is `+0x680`.** `+0x400` is not a
      defined sub-block and would have written buffer-translation values into unassigned space —
      silent, and it presents *after* the PLL locks, so it looks like a signal-integrity problem.
      §8.2 now states the full base-plus-sub-block rule plus the `TX = PCS + 0x80` mnemonic so every
      PHY offset is recomputable.
  11. **§2.1 contradicted §3.3 on forcewake.** §2.1 had read `NEEDS_FORCE_WAKE`'s `0x116000`
      threshold as "registers at or above this need forcewake", which swept in the combo PHY and
      DPLL configuration. §3.3 was right: `0x116000` is where a *fast-path filter* stops
      short-cutting, not where forcewake begins, and the authoritative `__gen12_fw_ranges` table
      assigns **domain 0** to the whole `0x40000`–`0x1BFFFF` range. Resolved in favour of §3.3, with
      the filter-versus-requirement distinction now spelled out and cross-referenced from both
      sections.
  12. **§6.3's "unresolved `PDIV`/`KDIV` discrepancy" was a false alarm of my own making** — I had
      compared `skl_wrpll_params_populate` (the *Skylake* encoder) against the *Gen12* decoder. They
      are not on the same path. ADL-N uses `icl_wrpll_params_populate`, which emits exactly the
      Gen12 named-constant values, so write and read round-trip. The section now documents the real
      finding instead: two populate functions, one shared struct, two incompatible encodings, with
      `P = 5` representable only in the Gen12 convention and `K = 5` only in the Skylake one.
  13. **§6.3 and §13.1 item 11 treated the wrpll search as an open gap.** It is not — ADL-N's search
      is `icl_calc_wrpll`, fully transcribed in §6.3, including the midpoint rule and the
      `icl_wrpll_get_multipliers` decomposition. The PRM's three DCO constants (7998 / 10000 / 8999
      MHz) match i915's three constants exactly.

- **A claim from the second review that I could not confirm, and did not adopt.** The review stated
  that "the PRM's stated bounds do not cover i915's dividers — `total = 35` has no PRM-legal
  `(P,Q,K)`, and `K = 5` is not a legal `K`". Both halves are true *of the Skylake divider list*,
  and both are false of the ADL-N path: `icl_calc_wrpll`'s list stops at 21 and contains no 35, and
  `icl_wrpll_get_multipliers` only ever emits `K ∈ {1,2,3}`. §6.3 records this explicitly rather
  than silently dropping it, because the Skylake list is a plausible place for a reader to end up.

- **Three sources of the same generation disagreed with each other on the power-well map** (TGL vs.
  RKL vs. `XE_LPD`). That is recorded in §4.2.1 and is the single strongest argument for verifying
  the map on real hardware rather than trusting any document — including this one.

- **A meta-lesson from defects 10–13, worth more than any of them individually.** Three of the four
  were the *same* mistake in different clothes: reaching for a Skylake-era or TGL-era symbol when
  the target is `XE_LPD`. The `skl_`/`icl_` prefix in i915's function names is not decorative —
  `skl_wrpll_*` and `icl_wrpll_*` take the same struct and mean different things by it, and
  `[TGL12]` documents a power-well map that ADL-N does not use. **When reading i915 for this
  platform, check the generation prefix on every helper before trusting its encoding.**
- **Licence.** This document records *facts* — register offsets, bitfield positions, sequences,
  constants, tables — which are not copyrightable. The GPL-2.0 `drm/i915` sources and the Intel
  PRMs were read for those facts; no code, comment or prose from either was reproduced.
  TheKernel is Apache-2.0 and this document is written to be usable by an independent
  implementation.
- **Ordering of value.** If you read only three things, read §4.4 (the power well handshake and its
  timeouts), §7.1 (watermarks are not optional), and §11.1 (the GMBUS failure table). Those three
  account for the majority of first-bring-up failures.
- **Then read §13.4 and go dump a working machine.** This document tells you what the registers
  probably are; the machine tells you what they actually are. The second is better.
