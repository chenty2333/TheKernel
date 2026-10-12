# Intel display framebuffer translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`tk-intel-display::intel_fb_full` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_fb.c` (MIT, 89/89 C functions;
approximately 2,365 Rust lines). It carries format/modifier checks, pitch and
plane-offset calculations, framebuffer extents, source rectangles in 16.16
fixed point and rotation/remap logic. GEM/frontbuffer/DPT allocation and
registration remain narrow kernel hooks.

The translated module is included in the display crate test build, including
tests for fixed-point rotation and C-style `u32` wrap behavior; kernel FB/KMS
creation and scanout paths have not yet been switched to it.
