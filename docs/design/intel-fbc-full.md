# i915 Framebuffer Compression source translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_fbc_full.rs` translates 126/134 ctags functions, in source order, from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_fbc.c` (MIT-style Intel grant; Copyright © 2014 Intel Corporation), in 2,533 Rust lines. It retains FBC eligibility, CFB/stolen-memory policy, register/write ordering, frontbuffer/dirty rectangle, flip/underrun, false-color, initialization and cleanup logic.

The omitted functions are `fbc_sys_cache_update_config`, `fbc_sys_cache_disable`, `fbc_sys_cache_limit`, and `fbc_sys_cache_enable` (display 35+/XE3P only), plus `intel_fbc_debugfs_status_show`, `intel_fbc_debugfs_add`, `intel_fbc_crtc_debugfs_add`, and `intel_fbc_debugfs_register` (DRM debugfs framework glue). `FbcRegisterIo`, `StolenAllocator`, `FbcControlHooks`, and `FbcPipeGraph` isolate MMIO/DSB, stolen allocator, DRM/workqueue/vblank/object, and CRTC/plane boundaries. The kernel has no active `FbcControlHooks` adapter yet, so this translation is not active scanout compression support. Full MIT grant is retained in `LICENSES/Intel-i915-FBC-MIT.txt`.
