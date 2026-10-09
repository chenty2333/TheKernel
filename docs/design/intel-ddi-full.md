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
`intel_ddi_buf_enable()` also owns the combo output's buffer write, posting read
and 10-ms active poll through a single-register `DdiIo` adapter. That
integration caught and fixed a translated sync-flag offset error and removed a
Gen13 `PORT_WIDTH` field that upstream only adds on display version 14+. The
policy adapter cannot access unrelated MMIO/PHY registers. A complete kernel
`DdiIo` binding and source DDI pre-enable/enable/disable call sequence are not
yet connected.
