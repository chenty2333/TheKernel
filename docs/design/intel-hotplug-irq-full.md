# Intel hotplug IRQ handling

`crates/ax/tk-intel-display/src/intel_hotplug_irq_full.rs` translates all 93
function definitions in Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_hotplug_irq.c` (MIT, Copyright © 2023 Intel
Corporation). The source markers cover every function, including the
display-generation and HPD-pin register masks, per-pin acknowledgement loop,
storm/recovery dispatch, and the common AUX/GMBUS IRQ wake paths.

`HotplugIrqIo` maps register access and kernel IRQ/HPD/DRM events. The native
N305 IRQ adapter now invokes the translated `gen11_hpd_irq_handler` for the
selected TC1/TC2 DE-HPD source, using its translated pin map and long-pulse
decode before publishing the deferred task-context notification. The IRQ
owner still retains its fail-closed source admission, W1C acknowledgements,
root re-enable ordering, and timer-backed connector polling; other generation
handlers, storm/recovery work, AUX/GMBUS IRQ wake paths, and full all-port
registration remain outside this adapter. `LICENSE-MIT` carries the upstream
grant.
