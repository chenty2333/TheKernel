# Intel vblank and scanout-position source translation

`crates/ax/tk-intel-display/src/intel_vblank_full.rs` translates all 30
function definitions from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_vblank.c` (MIT, © 2022-2023 Intel),
including both I915/Xe alternatives for the duplicated vblank-section lock
helpers. It preserves frame/pixel-counter vblank accounting, timestamp-based
scanline calculation, interlace conversions, VRR/DSI scanline handling,
critical-section ordering, active-timing update, and vblank evasion. Register,
IRQ, waitqueue, VRR and generic DRM timestamp operations are trait boundaries.
The module is exported and has focused mode/evade tests; kernel modeset/atomic
callers are not yet redirected to this source helper.
