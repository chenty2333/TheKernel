# i915 PSR/Panel Replay source translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_psr_full.rs` translates 155/155 ctags functions, in source order, from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_psr.c` (MIT; Copyright © 2014 Intel Corporation), in 4,487 Rust lines. It retains source generation branches and implements the display-12/13-relevant DPCD capability/configuration paths, source workarounds, PSR1/PSR2/Panel Replay setup, atomic pre/post update, frontbuffer and selective-update state, exit/disable/retrain, IRQ, and recovery sequencing.

`PsrIo` exposes DRM/atomic state enumeration, debugfs, runtime-PM, VBT, AUX/DPCD, PHY/ALPM/DMC, workqueue, DSB, and MMIO boundaries. No kernel `PsrIo` adapter is currently connected to N305's fastboot/modeset path; this module is a source-translation layer, not active PSR hardware support yet. The retained MIT grant is also in `LICENSES/Intel-i915-PSR-MIT.txt`.
