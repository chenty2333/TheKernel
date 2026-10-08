# Intel CDCLK and PLL transition work

`crates/ax/tk-intel-display/src/cdclk.rs` owns the table-driven ADLP CDCLK
selection and the platform-independent transition predicates. This change adds
the source conditions for `intel_cdclk_can_crawl()`,
`intel_cdclk_can_squash()`, `intel_cdclk_can_cd2x_update()`,
`intel_cdclk_clock_changed()`, and `cdclk_compute_crawl_and_squash_midpoint()`
from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT,
Copyright © 2006-2017 Intel). The full MIT text is in
`crates/ax/tk-intel-display/LICENSE-MIT`.

`CdclkConfig` carries the actual frequency, PLL VCO/reference and squash
waveform. `transition()` follows `_bxt_set_cdclk()`'s selection order: use a
crawl+squash midpoint where available, otherwise crawl, squash, update only
CD2X, or require a full PLL update. This is a pure planning layer and has unit
coverage; the kernel has not yet connected it to modeset commits.

Still unported from the upstream file are runtime MMIO sequencing and locking,
PCODE pre/post notifications, audio/PSR/GMBUS/AUX coordination, atomic CDCLK
state calculation, per-plane/bandwidth/watermark minima, maximum-frequency
readout, and debugfs. The existing kernel `clk.rs` remains the N305 boot-time
CDCLK/RAWCLK path; this change does not claim runtime clock switching is
enabled.
