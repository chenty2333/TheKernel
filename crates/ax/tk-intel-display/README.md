# tk-intel-display

MIT, `no_std`, allocation-free byte parsing and explicit injected MMIO.
Source authority is Linux 7.2.3 `drivers/gpu/drm/i915/display/`; see NOTICE
for **actually translated** functions and LICENSE-MIT for the full grant.

This first slice contains ADL-P/N identity/stepping and BDB 216–264 board
routing/HDMI capabilities plus OpRegion VBT discovery. It is not a complete
i915 port, hardware modeset driver, or firmware-ownership proof. No register
backend or framebuffer mapping is implicit; parsing cannot touch hardware.

Intentional safety divergences: validate each accessed section boundary and header/signature
sizes; reject incomplete child tails; duplicate-port lookup errors rather than
silently choosing the first; cap external VBT to 64 KiB and reject overlapping
2.1+ RVDA or address overflow. Checksum is observable but, matching i915, not
mandatory for parsing. Unsupported semantic BDB versions return an error;
short children use upstream zero-extension but report unexpected record size.
Unknown stepping preserves upstream next/future decoding but is marked inexact.

Private BIOS/EDID bytes are not licensed for redistribution by their presence
in a capture. The external test reads them directly, does not bundle them:

```
THEKERNEL_N305_CAPTURE=/path/to/n305-20261003T235530Z \
  cargo test -p tk-intel-display --target x86_64-unknown-linux-gnu \
  --test n305_capture -- --ignored --nocapture
```

Synthetic malformed tables and board-routing regressions run in the normal
host suite. Actual captured tests are separate and never silently count a
missing capture as a pass. 未在硬件上验证.

`display.rs` translates the non-DSI display-13 transcoder timing reads in exact
upstream order, including interlace correction followed by SET_CONTEXT_LATENCY
vblank-start override, and the distinct high-half PIPESRC width decode. A backend
must hold a stable already-powered pipe domain; sleeping readout refuses without
MMIO. The kernel's narrow adapter only reads admitted live pipe A before the
first modeset write. It cannot write through RegisterIo and does not open the
existing combo-only transaction to TC. **This is not complete firmware state
readout or fastboot takeover.** PLL, format/modifier, scaling, watermark/color,
GGTT ownership and full equivalence remain pending.

The optional `upstream_readout` test extracts unmodified
`intel_get_transcoder_timings` and register definitions from the local reference,
compiles them with a minimal read backend, and compares 128 raw states across
four transcoders/progressive+interlaced, including the exact seven-read order.
It needs `THEKERNEL_LINUX_REFERENCE`, `THEKERNEL_STATE_DIR` under `/home`, and
GCC; run it explicitly with `--test upstream_readout -- --ignored`. Its generated
C/binary is removed at test completion; no independent-oracle success is inferred
from the handwritten unit model. This oracle checks timing only, not ownership.

`dpll_mgr.rs` now includes pure DKL HDMI/non-SSC divisor calculation and
frequency readback. It preserves source divisor search order, integer fractional
truncation, all selected clock/PLL register words and optional AFC startup bits.
The `upstream_clock` oracle compares 192 clock/refclk/AFC cases to unmodified
compiled C, plus 42 ADL-P CDCLK table selections. CDCLK only quantizes a supplied
**global** minimum; pixel-rate/2 alone is not the complete watermark/DBUF/audio
minimum. Unsatisfiable requests fail instead of C's warning+max fallback; A0 or
unknown stepping is not admitted. No TC PLL, CDCLK, DKL HIP selector, PHY/power
or workaround register programming is implemented by these arithmetic functions.
