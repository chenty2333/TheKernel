# Intel CRTC state, events and pipe-update translation

`crates/ax/tk-intel-display/src/intel_crtc_full.rs` translates the 39 ctags
function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_crtc.c`
(MIT, © 2020 Intel). It covers CRTC init/cleanup policy and generation dispatch,
vblank counter limits, state sentinels, atomic plane registration order,
pipe-update start/end and vblank events, worker/QoS policy, pipe reorder, and
bandwidth/CDCLK helpers. DRM object allocation/registration, PSR/VRR, atomic
state iteration, vblank work, IRQ and platform services are explicit traits.
The module is exported and compiles with focused policy tests; current kernel
KMS initialization and atomic update paths are not routed through it yet.
