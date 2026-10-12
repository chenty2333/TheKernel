# Intel Skylake-generation scaler translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/skl_scaler_full.rs` translates all 43 function
definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_scaler.c`
(MIT, © 2020 Intel), preserving scaler allocation/staging order, phase and
filter programming, plane/pfit validation, register sequencing, readback,
ECC-mask workaround, and prefill-limit helpers. DRM state lookup, tracing,
CASF, DSB and MMIO are explicit `ScalerIo` hooks. The module is exported and
passes crate unit tests; kernel plane/atomic call sites still use the existing
narrow scaler path and have not yet been switched to this full state.
