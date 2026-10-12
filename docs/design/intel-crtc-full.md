# Intel CRTC state, events and pipe-update translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_crtc_full.rs` translates the 39 ctags
function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_crtc.c`
(MIT, © 2020 Intel). It covers CRTC init/cleanup policy and generation dispatch,
vblank counter limits, state sentinels, atomic plane registration order,
pipe-update start/end and vblank events, worker/QoS policy, pipe reorder, and
bandwidth/CDCLK helpers. DRM object allocation/registration, PSR/VRR, atomic
state iteration, vblank work, IRQ and platform services are explicit traits.
The module is exported and compiles with focused policy tests; current kernel
KMS initialization and atomic update paths are not routed through it yet.
