# Intel DisplayPort policy and configuration

`crates/ax/tk-intel-display/src/intel_dp_full.rs` translates 284/287 function
definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dp.c` (MIT,
Copyright © 2008 Intel Corporation). Coverage includes display-12/13 source and
sink rates, DPCD/LTTPR and MST/ESI state, detect/EDID/eDP and downstream PCON
flows, DSC mode/link bandwidth selection, SDP framing, and configuration
ordering. Only two generic DRM property attachment wrappers and the
display-14+-only `mtl_max_source_rate()` are omitted.

The source-sized mechanism is composed of `DpAuxIo`, `DpFramework`, and mode /
PCON hooks. `intel_dp_link_training_full.rs` carries the separate link-training
translation; `dp_aux.rs` owns the AUX transport mechanics. The kernel's A/B
polling adapter now supplies a DPCD capability read to connector reporting, but
this module's bandwidth configuration and link training are not yet called by
the modeset path, and its full PHY/TC/PSR/HDCP framework implementations are
not present. Its crate tests exercise the platform source-rate selection,
coding clock, and conservative sink fallback.
The full MIT text is in `crates/ax/tk-intel-display/LICENSE-MIT`.
