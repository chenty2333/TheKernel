# Intel DisplayPort policy and configuration

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

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
polling adapter now reads the 16-byte DPCD receiver-capability block and, when
the DP 1.3 extended-capability bit is set, reads/selects the extended block by
the upstream DRM revision rule. Connector reporting decodes the standard
maximum link-rate/lane-count fields and labels them informational only. This
module's bandwidth configuration
and link training are not yet called by the modeset path, and its full
PHY/TC/PSR/HDCP framework implementations are not present. Its crate tests
exercise the platform source-rate selection, coding clock, and conservative
sink fallback.
The full MIT text is in `crates/ax/tk-intel-display/LICENSE-MIT`.
