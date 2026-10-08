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
CD2X, or require a full PLL update. The kernel `clk::transition()` adapter now
executes the Gen12 ratio/enable/lock or crawl/request/ack sequence and writes
`CDCLK_CTL`, retaining i915's warning-only PLL poll outcomes. Host unit tests
cover the pure plan and compile-check covers the kernel adapter; its modeset
call site is not yet connected.

Still unported from the upstream file are caller-side PCode pre/post
notifications, audio/PSR/GMBUS/AUX locking and coordination, atomic CDCLK
state calculation, per-plane/bandwidth/watermark minima, maximum-frequency
readout, and debugfs. The existing `bring_up()` remains the N305 boot-time
CDCLK/RAWCLK entry point; runtime transition code is present but not invoked by
a modeset caller yet.
