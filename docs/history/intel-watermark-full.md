# Intel DBUF and watermark source translation

`crates/ax/tk-intel-display/src/skl_watermark_full.rs` is a source-ordered
translation of Linux 7.2.3 `drivers/gpu/drm/i915/display/skl_watermark.c`
(MIT, © 2022 Intel), with all 140 ctags function definitions represented.
It includes ICL/TGL/DG2/ADLP DBUF slice tables, DDB allocation and watermark
compute/update paths, SAGV, readback, latency, MBUS, prefill and state
verification. DRM atomic objects, PCODE, MMIO/register access, logging and
debugfs are expressed through narrow traits; Rust value-state adapters replace
Linux object lifecycle helpers. The module is exported and compiled by crate tests. The active N305 pipe-A
primary-plane path now calls `skl_build_plane_wm_single()` with PCode-derived
latencies and SAGV block time, and applies the source DDB-fit check. Multi-pipe
DBUF allocation, cursor/overlay/scaler plane watermarks, atomic transitions,
and SAGV control remain outside that single-primary-plane adapter.
