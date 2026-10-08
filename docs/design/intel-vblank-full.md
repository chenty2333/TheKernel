# Intel vblank and scanout-position source translation

`crates/ax/tk-intel-display/src/intel_vblank_full.rs` translates all 30
function definitions from Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_vblank.c` (MIT, © 2022-2023 Intel),
including both I915/Xe alternatives for the duplicated vblank-section lock
helpers. It preserves frame/pixel-counter vblank accounting, timestamp-based
scanline calculation, interlace conversions, VRR/DSI scanline handling,
critical-section ordering, active-timing update, and vblank evasion. Register,
IRQ, waitqueue, VRR and generic DRM timestamp operations are trait boundaries.
The native N305 DRM adapter now uses the translated
`i915_get_vblank_counter` for its reported Pipe-A counter. It performs the
source's stable PIPEFRAME/PIPEFRAMEPIXEL/PIPEFRAME read, applies the active
timing's hsync boundary adjustment, and marks the adapter lost if a register
sample is unreadable or cannot be stabilized. The legacy raw Pipe-A frame read
remains only for internal buffer-latch progress checks, where the driver needs
to observe that a new frame occurred rather than expose the DRM vblank value.
