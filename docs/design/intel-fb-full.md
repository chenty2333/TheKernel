# Intel display framebuffer translation

`tk-intel-display::intel_fb_full` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_fb.c` (MIT, 89/89 C functions;
approximately 2,365 Rust lines). It carries format/modifier checks, pitch and
plane-offset calculations, framebuffer extents, source rectangles in 16.16
fixed point and rotation/remap logic. GEM/frontbuffer/DPT allocation and
registration remain narrow kernel hooks.

The translated module is included in the display crate test build, including
tests for fixed-point rotation and C-style `u32` wrap behavior; kernel FB/KMS
creation and scanout paths have not yet been switched to it.
