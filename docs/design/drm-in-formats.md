# DRM plane `IN_FORMATS` property

The primary and cursor planes now expose immutable `IN_FORMATS` blobs through
`OBJ_GETPROPERTIES`/`GETPROPBLOB`. The blob uses the Linux
`drm_format_modifier_blob` layout (version 1), advertises only DRM linear
modifier zero, and matches the actual `ADDFB2` checks: primary supports
XRGB8888/ARGB8888, cursor supports ARGB8888. Static device-owned blob IDs do
not overlap per-device user-created blobs.

`cargo check -p tk-kernel --tests --features 'intel-hda nvme watchdog-itco bpf'`
passes. The crate's `property.rs` includes unit tests for exact offsets and
format bitmasks; the bare-metal kernel test binary is not linkable on the host
because of the existing per-CPU absolute-relocation limitation, so those tests
were compile-checked but not executed.

This does not add tiled Intel modifiers, multi-plane formats or multi-CRTC
resources; only layouts the current framebuffer/scanout path actually accepts
are advertised.
