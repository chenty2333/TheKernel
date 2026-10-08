# DRM plane helper translation

`kernel::drm::plane_uapi_full` translates all 38 function definitions from
Linux 7.2.3 `drivers/gpu/drm/drm_plane.c` (MIT-style grant, 1,394 Rust lines).
It covers primary/cursor plane setup and cleanup, format/modifier constraints,
property get/set, legacy plane/cursor/page-flip UAPI, damage, scaling/size and
atomic plane state paths. DRM objects, usercopy, locks, FB/CRTC references,
atomic-state lifetimes, vblank/events and driver callbacks are `DrmPlaneIo`
hooks.

The module is declared and compile-checked, but no `DrmPlaneIo` adapter yet
routes the active kernel plane ioctl/KMS path through it. The existing direct
KMS path remains active. The upstream file has no rotation algorithm beyond
the UAPI property/state transport; this translation does not invent one.
