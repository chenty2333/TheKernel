# Intel hotplug IRQ handling

`crates/ax/tk-intel-display/src/intel_hotplug_irq_full.rs` translates all 93
function definitions in Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_hotplug_irq.c` (MIT, Copyright © 2023 Intel
Corporation). The source markers cover every function, including the
display-generation and HPD-pin register masks, per-pin acknowledgement loop,
storm/recovery dispatch, and the common AUX/GMBUS IRQ wake paths.

`HotplugIrqIo` maps register access and kernel IRQ/HPD/DRM events. The module is
compiled into the display crate but does not yet have a kernel implementation
connected to `kernel/src/drm/intel/irq.rs`; the existing N305 IRQ path remains
the live one. `LICENSE-MIT` carries the upstream grant.
