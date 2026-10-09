# Intel modeset setup translation

`tk-intel-display::intel_modeset_setup_full` translates all 25 function
definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_modeset_setup.c`
(MIT, 1,015 source lines; 848 Rust lines). The source-order module ports
initial CRTC/encoder state discovery, pipe/transcoder assignment and modeset
setup decisions. DRM state/object enumeration and diagnostic hooks are kept
behind the translated call boundaries.

It is compiled as part of `tk-intel-display`, but current kernel connector
probe/modeset setup still uses the existing path rather than this module.
