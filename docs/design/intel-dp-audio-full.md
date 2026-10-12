# i915 DP audio source translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/intel_audio_dp_full.rs` translates the Display-12/13 DP-audio and shared audio logic from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_audio.c` (MIT, Copyright © 2014 Intel Corporation). With `intel_audio_legacy_remainder.rs`, the two modules cover all 45 ctags functions in source order: the remainder adds the three HDMI helpers, four G4x helpers, and three IBX helpers.

DP audio configuration, ELD/codec handling and sequencing are represented in Rust. `IntelAudioDpHooks` and `LegacyAudioIo` are boundaries for audio-core, HDA, ELD storage, MMIO, mutexes, vblank and DRM framework operations. The existing kernel audio/ELD session remains the active N305 path; these source modules are not yet connected to that path.
