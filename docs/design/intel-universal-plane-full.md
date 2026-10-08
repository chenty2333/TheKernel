# Intel universal-plane source translation

`crates/ax/tk-intel-display/src/skl_universal_plane_full.rs` translates all
112 function definitions in Linux 7.2.3
`drivers/gpu/drm/i915/display/skl_universal_plane.c` (MIT, © 2020 Intel), in
source order. It covers display-12/13 format/modifier admission, surface and
CCS/UV offset calculation, plane checks, WM/control/color/CSC programming,
update arm/noarm sequences, plane creation policy, and initial state
readback/fixup. DRM framebuffer/atomic clipping helpers, BO-protection queries,
plane/IRQ registration, DSB/MMIO and power are explicit traits; source
algorithms and branch order remain in the translated functions. The module is
exported and compiled by crate tests. Kernel plane/atomic paths still use
existing narrower code and are not switched to this state yet.
