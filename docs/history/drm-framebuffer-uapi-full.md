Historical: describes the kernel-native DRM core removed in bc5bc89a.

# DRM framebuffer UAPI source translation

`kernel/src/drm/framebuffer_uapi_full.rs` translates all 27 functions in Linux
v7.2.3 `drivers/gpu/drm/drm_framebuffer.c` (835 Rust lines), in source order.
The C file has no SPDX line; its Intel header's permissive grant was retained
verbatim in the historical translation rather than being reclassified as SPDX
MIT. The separate kernel license-text copy was retired with that removed code.
Function markers match ctags order exactly (27/27).

Framebuffer creation, format/modifier checks, cleanup and references are
expressed through the `FramebufferUapiIo` framework boundary. The module
compiles with the kernel crate but remains a reference translation: current
ADL-N KMS `ADDFB2`/plane code is separate and still advertises linear XRGB8888
only; this source module is not yet the live framebuffer implementation.
