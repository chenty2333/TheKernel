# Intel display color translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`tk-intel-display::intel_color_full` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_color.c` (MIT, 223/223 C functions;
approximately 3,600 Rust lines). It preserves the upstream color LUT, gamma,
degamma, CTM coefficient, CSC selection, register-value and programming-order
algorithms. DRM properties/atomic-state ownership and DSB/MMIO are explicit
interfaces rather than copied DRM core.

The module is compiled and its focused LUT tests run as part of the crate suite,
but is not yet wired into TheKernel's atomic color-commit path. Consequently
the translated policy is not active for userspace modesets yet.
