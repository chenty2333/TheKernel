# i915 CDCLK source translation

`crates/ax/tk-intel-display/src/intel_cdclk_full.rs` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_cdclk.c` (MIT, Intel, 152/152 ctags
functions, source order) with no source function omissions. Register/PCI
access, PCODE, platform state, atomic DRM operations, waits, and diagnostics
are required `IntelCdclkIo` backend methods rather than success defaults.

The crate exports the module and `cargo check -p tk-intel-display --lib` plus
`cargo test -p tk-intel-display --lib` compile the translation; the latter's
149 tests pass. The parameterized raw-clock and PLL-ratio macros are expanded
from the matching `intel_display_regs.h` definitions, and the CNP fractional
raw-clock fields have a focused test. The kernel currently uses its separate CDCLK bring-up and
runtime-MMIO adapters, but does not yet invoke this full source module or
connect the atomic runtime transition to the active modeset path. That caller
integration remains part of the clock task.
