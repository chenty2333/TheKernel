# Intel modeset verification translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`tk-intel-display::intel_modeset_verify_full` translates all 7 function
definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_modeset_verify.c`
(MIT, 256 source lines; 509 Rust lines). It ports connector/encoder/CRTC pipe
state verification, disabled-state checks, pipe-config sanity and watermark,
DPLL/MPLLB diagnostic dispatch. DRM object traversal, state readout and logging
remain hooks.

The N305 fastboot TC transaction now invokes translated
`intel_modeset_verify_crtc()` after its target capture and before publishing
the new mode. Its adapter checks the captured connector route, active state,
Pipe-A assignment, and pixel clock; any source warning becomes a rollback
failure rather than a log-only result. The existing full `same_mode_state`
check and source DPLL readout remain authoritative for the remaining register,
PHY, watermark, and DPLL details; unsupported generic verifier hooks do not
pretend to be implemented. Product-feature `cargo check -p tk-kernel --tests`
passed with this adapter. The worker's standalone check found/fixed a lifetime
issue but was not rerun under its one-check limit.
