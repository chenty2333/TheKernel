# DRM mode-config helper translation

`kernel::drm::mode_config_full` translates all 14 function definitions in
Linux 7.2.3 `drivers/gpu/drm/drm_mode_config.c` (MIT-style Intel grant, 861
Rust lines). It ports mode-config property initialization including CTM,
degamma/gamma LUT properties and sizes, CRTC/encoder/plane limits, resource
enumeration, init/cleanup order, and registration unwind behavior. DRM object
registration, free/locks and userspace callbacks remain explicit hooks.

`drm_mode_config.c` itself contains no EDID reader; connector/EDID behavior is
in `drm_connector.c` and represented by the separate connector module. This
translated file is declared and compile-checked, but current mode-config
initialization still uses the existing kernel path; a `ModeConfigIo` adapter
has not been wired.
