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
`intel_cdclk_can_squash()`, `intel_cdclk_can_cd2x_update()`,
`intel_cdclk_clock_changed()`, and `cdclk_compute_crawl_and_squash_midpoint()`
from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT,
Copyright © 2006-2017 Intel). The full MIT text is in
`crates/ax/tk-intel-display/LICENSE-MIT`.

Initial power-up also calls translated `icl_calc_voltage_level()` for the
PCODE level instead of keeping a second local threshold chain. The policy-only
backend performs no MMIO and reports an unsupported out-of-range clock before
committing the mailbox update.

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
modeset failure. Native TC HDMI now also carries a restricted runtime callback
in its Pipe-A transaction: when a requested pixel clock exceeds the current
CDCLK, it requires every PIPECONF, transcoder FUNC_CTL and DDI buffer window to
read disabled while the transaction has quiesced the link, then raises to the
minimum source table row with PCode prepare/voltage and restores the captured
clock during verified rollback. An inaccessible peer DDI buffer fails closed.
This is raise-only for the single selected TC path; general active-pipe atomic
CDCLK changes, lowering/power optimization, and generic modeset orchestration
remain unconnected. The current Native mode set (firmware 4K30 and 1080p60)
is not expected to cross its boot CDCLK ceiling, so this callback is integrated
but not exercised by those presently advertised timings.

Still unported from the upstream file are caller-side PCode pre/post
notifications, audio/PSR/GMBUS/AUX locking and coordination, atomic CDCLK
state calculation, per-plane/bandwidth/watermark minima, maximum-frequency
readout, and debugfs. The existing `bring_up()` remains the N305 boot-time
CDCLK/RAWCLK entry point; the full translation's `IntelCdclkIo` backend is not
yet wired. The opt-in rollback transaction still denies generic CDCLK writes
and journals PCODE side effects only through its explicit clock-transition
method, which pairs the source order and reverse recovery.
