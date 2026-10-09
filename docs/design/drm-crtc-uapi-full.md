# DRM CRTC UAPI source translation

`kernel/src/drm/crtc_uapi_full.rs` translates all 26 function definitions in
Linux v7.2.3 `drivers/gpu/drm/drm_crtc.c` (1,344 Rust lines). The upstream file
has no SPDX tag; its Intel/Dave Airlie/Red Hat copyright header contains an
explicit permissive grant, retained in the translation and
`kernel/LICENSES/LicenseRef-Intel-Drm-Crtc-MIT`.

CRTC UAPI sequencing, validation, cleanup and callback ordering are preserved
behind the `CrtcUapiIo` framework boundary. Exact upstream function markers
match ctags source order (26/26); the module is compile-checked as part of the
kernel crate. It is currently a translated reference module, not yet the
implementation behind the live Native CRTC ioctl path. The generic DRM state
and active Intel scanout remain single-CRTC/one-pipe; no multi-CRTC resources
are advertised by this addition.
