# Display-13 TC HDMI clock arithmetic slice

2026-10-05。Source: Linux 7.2.3 i915 MIT files, function inventory in
`crates/ax/tk-intel-display/NOTICE`. **Partial D2/D3, 未在硬件上验证.**

The capture's HDMI uses DDI TC1, so the source calculation is the DKL branch
of `icl_calc_mg_pll_state`, not the combo WRPLL used by the old kernel port.
The Rust slice preserves `icl_mg_pll_find_divisors` priority `{7,5,3,2}`, then
DS divider 10 down to 1, HDMI DCO range 7.992–10 GHz, M1=2, M2 22-bit fraction,
reference trim/TDC coefficients/feed-forward, optional AFC startup and all
selected register encodings. Readback uses the same delayed fractional divide.
TMDS clock must come from HDMI compute_config (bpc/YUV420); it is not always
pixel clock. This slice's captured 1080p case is 8-bpc RGB at 148500 kHz.

For 148500 TMDS and a **test input** 19200 kHz reference, source and Rust agree:

| Field | Value |
|---|---:|
| DCO | 8910000 kHz |
| MG_REFCLKIN_CTL | 0x100 |
| MG_CLKTOP2_CORECLKCTL1 | 0x500 |
| MG_CLKTOP2_HSCLKCTL | 0x15400 |
| DKL_PLL_DIV0 | 0x842e8 |
| DKL_PLL_DIV1 | 0x1c004f |
| DKL_PLL_SSC | 0x20002000 (SSC disabled) |
| DKL_PLL_BIAS | 0x42000000 |
| DKL_PLL_TDC_COLDST_BIAS | 0x4a |
| Readback TMDS | 148500 kHz |

The capture does not establish the next boot's reference clock. Tests cover
19200/24000/38400 rather than assume 19200. The clock functions do not map MMIO,
write HIP/DKL registers, acquire TC cold/power ownership, apply stepping WAs,
select/lock a PLL or expand rollback. Those load-bearing stages remain pending.
AFC `None` in the capture arithmetic test is a test configuration, not proof
that the actual VBT driver-feature policy never overrides AFC. Upstream C
matrix independently covers overrides 0/3/7 as well.

`cdclk.rs` ports `adlp_cdclk_table` for B0+/D0, pixel-rate minimum with PPC=2
and guardband=100, and first adequate entry selection. Inputs are a measured
reference and a precomputed **global** minimum/fuse maximum. It deliberately
refuses A-step/future stepping and unmet limits, rather than emulate C's
warning-and-return-max on impossible requests. It does not compute plane,
memory-bandwidth, audio/DBUF/watermark requirements or PCODE voltage, crawl and
CDCLK writes. At 148500 pixel rate, 74250 kHz is only the pixel-rate lower bound;
this cannot justify lowering the captured 192000 CDCLK. The existing kernel
transaction continues to preserve firmware CDCLK.

## Measured host validation

- 21 normal crate unit/readout tests cover encodings, ranges, unsupported
  inputs, fractional 148352→148351 readback, table boundaries and no-write readout.
- Compiled unmodified i915 C oracle: 192 DKL HDMI clock/reference/AFC cases,
  including rejected low clocks, and 42 CDCLK selections match all output fields.
- Timing C oracle: 128 raw states match all timing fields and the seven-read order.
- The separate private-capture test uses the existing TheKernel EDID parser
  and native mode policy with a preserved 192000 CDCLK ceiling, verifies advertised
  148500/1920/2008/2052/2200 and 1080/1084/1089/1125, checks TC1/GMBUS9 in VBT,
  calculates DKL clock/readback for all three reference inputs and preserves
  captured 192000 CDCLK. Its execution result belongs in progress-D, not inferred
  from the expected values in this document. The EDID base/CTA checksums are
  valid, but its EDID 1.3 range descriptor uses the 1.4-only bare-limits form.
  Strict parse reports InvalidDisplayDescriptor(2); existing explicit lossy
  parsing skips that descriptor with a warning. Preferred 4K30 exceeds the
  old native policy ceiling, so its advertised 1080p60 fallback is selected.
  No parser validation was relaxed and no strict-parse success is claimed.

These are arithmetic/readout tests, **not a full upstream EDID parser differential,
complete atomic clock policy, native HDMI programming or physical PLL/scanout proof**.
C tests require local Linux and GCC and are explicitly marked external tests; missing
inputs never count as a pass. BIOS/capture bytes remain outside Git. The generated
host C oracle is removed at completion, not shipped as a new runtime implementation.
