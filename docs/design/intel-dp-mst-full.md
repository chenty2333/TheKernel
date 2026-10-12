# i915 Display-12/13 DP MST source translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_dp_mst_full.rs` translates all 68 ctags function definitions, in source order, from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_dp_mst.c` (MIT; Copyright © 2008 Intel Corporation and 2014 Red Hat Inc.). Link bandwidth, payload arithmetic, atomic state policy, connector/stream sequencing and source predicates are implemented in Rust. DRM MST topology/object lifetime, sideband transport, AUX, MMIO and frame/event operations are exposed through `MstBackend` rather than importing Linux DRM internals.

The module compiles under the `tk-intel-display` crate and includes source-order `upstream:` markers. No kernel `MstBackend` adapter or MST registration/resource plumbing is connected to the active N305 KMS path yet; the existing kernel connector model remains single-connector/single-CRTC.
