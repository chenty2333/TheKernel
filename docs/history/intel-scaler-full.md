# Intel Skylake-generation scaler translation

`crates/ax/tk-intel-display/src/skl_scaler_full.rs` translates all 43 function
definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_scaler.c`
(MIT, © 2020 Intel), preserving scaler allocation/staging order, phase and
filter programming, plane/pfit validation, register sequencing, readback,
ECC-mask workaround, and prefill-limit helpers. DRM state lookup, tracing,
CASF, DSB and MMIO are explicit `ScalerIo` hooks. The module is exported and
passes crate unit tests; kernel plane/atomic call sites still use the existing
narrow scaler path and have not yet been switched to this full state.
