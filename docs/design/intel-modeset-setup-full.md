# Intel modeset setup translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`tk-intel-display::intel_modeset_setup_full` translates all 25 function
definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_modeset_setup.c`
(MIT, 1,015 source lines; 848 Rust lines). The source-order module ports
initial CRTC/encoder state discovery, pipe/transcoder assignment and modeset
setup decisions. DRM state/object enumeration and diagnostic hooks are kept
behind the translated call boundaries.

It is compiled as part of `tk-intel-display`, but current kernel connector
probe/modeset setup still uses the existing path rather than this module.
