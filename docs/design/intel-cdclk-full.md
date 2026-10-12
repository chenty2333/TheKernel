# i915 CDCLK source translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_cdclk_full.rs` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT, Intel, 152/152 ctags
functions, source order) with no source function omissions. Register/PCI
access, PCODE, platform state, atomic DRM operations, waits, and diagnostics
are required `IntelCdclkIo` backend methods rather than success defaults.

The crate exports the module and `cargo check -p tk-intel-display --lib` plus
`cargo test -p tk-intel-display --lib` compile the translation; the latter's
150 tests pass. The parameterized raw-clock and PLL-ratio macros are expanded
from the matching `intel_display_regs.h` definitions, and the CNP fractional
raw-clock fields have a focused test.

The kernel does not yet implement an `IntelCdclkIo` backend for this full
module. Its existing typed runtime adapter is now called by the explicit
`intel.modeset=1` boot transaction only when every pipe and the combo DDI link
read disabled, the current CDCLK is a usable table row, and the chosen mode
exceeds that rate.
It raises to the lowest table row at or above the conservative one-pixel
clock ceiling, executes PCode PREPARE → clock transition/readback → PCode
voltage update, and records that the rollback transaction must reverse the
clock and restore the old PCode voltage if a later step fails. Generic writes
still reject CDCLK; active-pipe atomic transitions and complete bandwidth /
watermark-derived minima remain unconnected.
