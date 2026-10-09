# Intel DDI source translation

`crates/ax/tk-intel-display/src/intel_ddi_full.rs` is a source-ordered Rust
translation of Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_ddi.c` (MIT,
2012 Intel copyright), with 208/208 function definitions represented. Register
selection, buffer setup and wait semantics, transcoder control, DP link/FEC
helpers, output state readback and init planning follow the source ordering.
DRM object registration and access to clocks, power, PHY, AUX, panel, audio and
hotplug collaborators are explicit `DdiIo` hooks rather than fabricated
services. The file is exported by `tk-intel-display` and compiled by its crate
tests; the live combo-PHY output path now uses the source
`intel_ddi_transcoder_func_reg_val_get()` to compose `TRANS_DDI_FUNC_CTL` from
the admitted HDMI/DVI encoder and Pipe-A state. The source
`intel_ddi_enable_transcoder_func()` now writes `TRANS_DDI_FUNC_CTL2(A)` before
`TRANS_DDI_FUNC_CTL(A)` and the kernel verifies both readbacks. The source
`intel_ddi_buf_enable()` also owns the combo output's buffer write, posting read
and 10-ms active poll through a single-register `DdiIo` adapter. The bounded
adapters cannot access unrelated MMIO/PHY registers. This integration caught
and fixed a translated sync-flag offset error and removed a Gen13 `PORT_WIDTH`
field that upstream only adds on display version 14+. The active Pipe-A TC
transaction now uses the translated `intel_ddi_disable_transcoder_func()` to
clear FUNC_CTL2/FUNC_CTL, with a two-register allowlist and readback checks.
The same Pipe-A TC transaction calls `intel_ddi_enable_transcoder_func()` to
construct and write FUNC_CTL2/FUNC_CTL for TC1/TC2 selectors, with source
readback verification. Its surrounding PLL, PHY and TC ownership stages remain
the existing bounded legacy path. A complete kernel `DdiIo` binding and source
DDI pre-enable/enable/disable call sequence are not yet connected.
The kernel register-model test covers TC1 FUNC_CTL2-before-FUNC_CTL ordering,
polarity/port encoding and the matching disable masks; it type-checks with the
kernel test target, while execution is blocked by the current host linker’s
bare-metal per-CPU `R_X86_64_32S` relocations.
The active TC transaction also calls the translated `intel_ddi_buf_disable()`
and `intel_ddi_buf_enable()` for the source D/E selectors, preserving the
source posting-read and 10-ms idle/active handshakes. Their adapter is bounded
to the one selected DDI_BUF_CTL and does not expose DP FEC or PHY registers;
the model test covers the source wait paths and writes.
It also calls `intel_ddi_enable_transcoder_clock()` for transcoder A using the
display-13 TC1/TC2 PHY_F/PHY_G mapping; the checked clock-select readback
replaces a local register-value formula.
