# Intel CDCLK and PLL transition work

`crates/ax/tk-intel-display/src/intel_cdclk_full.rs` now translates the full
Linux 7.2.3 `intel_cdclk.c`: 152/152 ctags functions in source order. Its
`IntelCdclkIo` backend makes MMIO, PCI, PCODE, atomic and platform operations
explicit. It is exported and compiles as part of `tk-intel-display`.

`crates/ax/tk-intel-display/src/cdclk.rs` owns the table-driven ADLP CDCLK
selection and the platform-independent transition predicates. The active
N305 boot-mode selector now calls translated `intel_cdclk_full::bxt_calc_cdclk`
against the ADLP table to choose the minimum source clock, then resolves that
frequency through the existing reference/ratio table before allowing any
programming. Its tiny `IntelCdclkIo` policy backend has no register access and
fails closed if the source table cannot satisfy the request. This does not
wire runtime atomic CDCLK transitions. The module also adds
the source conditions for `intel_cdclk_can_crawl()`,
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
`CDCLK_CTL`, retaining i915's warning-only PLL poll outcomes. The opt-in N305
boot modeset now invokes the adapter only after all four pipes and the combo
DDI link are proven off,
with PCode PREPARE / voltage update around the change and a reverse transaction
path restoring both clock registers and the old PCODE voltage on later
modeset failure. Active-pipe atomic CDCLK transitions remain unconnected.

Still unported from the upstream file are caller-side PCode pre/post
notifications, audio/PSR/GMBUS/AUX locking and coordination, atomic CDCLK
state calculation, per-plane/bandwidth/watermark minima, maximum-frequency
readout, and debugfs. The existing `bring_up()` remains the N305 boot-time
CDCLK/RAWCLK entry point; the full translation's `IntelCdclkIo` backend is not
yet wired. The opt-in rollback transaction still denies generic CDCLK writes
and journals PCODE side effects only through its explicit clock-transition
method, which pairs the source order and reverse recovery.
