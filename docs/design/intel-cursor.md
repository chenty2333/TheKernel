# ADL-N cursor plane preparation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`kernel/src/drm/intel/cursor.rs` builds a bounded Pipe-A cursor register plan
through the translated Linux 7.2.3 `intel_cursor.c` policy helpers. It currently
accepts only linear 64x64 ARGB8888 surfaces, checks GGTT alignment and address
range, validates signed/hotspot-adjusted coordinates, and requires a caller
supplied six-level cursor watermark/DDB allocation. The helper does not issue
MMIO.

This is not yet a live KMS cursor: the Intel adapter continues to report no
cursor support. The current primary-plane path allocates the whole DBUF to the
primary, does not carry cursor framebuffer format/pitch or an explicit
disable-vs-unchanged state through `CursorUpdate`, and does not retain a
hardware cursor mapping through the next real vblank. Until per-plane DBUF/WM,
cursor ownership, before-image/rollback and retire signaling are integrated,
enabling the plane would be unsafe.
