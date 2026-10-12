# Intel display hotplug state machine

`crates/ax/tk-intel-display/src/intel_hotplug_full.rs` translates 37/44
functions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hotplug.c`
(MIT, Copyright © 2015 Intel). The seven omitted functions are debugfs
show/write/open/registration wrappers for storm controls. The translated paths
retain HPD storm detection and recovery, pin block/unblock counts, IRQ and
workqueue ordering, connector detection/retry/polling, and missed-IRQ handling.

`HotplugIo` represents the kernel IRQ/mode-config locks, HPD setup/pulse,
delayed work, runtime/display-core power references and DRM connector events.
The existing kernel `hpd.rs` still owns its prior detection path; no concrete
`HotplugIo` adapter currently connects this source state machine to that path,
so the translation is not yet an active IRQ/hotplug implementation. The full MIT
grant is present in the module header and `LICENSE-MIT`.
