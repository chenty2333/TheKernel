# Intel display color translation

`tk-intel-display::intel_color_full` translates Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_color.c` (MIT, 223/223 C functions;
approximately 3,600 Rust lines). It preserves the upstream color LUT, gamma,
degamma, CTM coefficient, CSC selection, register-value and programming-order
algorithms. DRM properties/atomic-state ownership and DSB/MMIO are explicit
interfaces rather than copied DRM core.

The module is compiled and its focused LUT tests run as part of the crate suite,
but is not yet wired into TheKernel's atomic color-commit path. Consequently
the translated policy is not active for userspace modesets yet.
