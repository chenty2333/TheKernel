# Intel cursor-plane source translation

`crates/ax/tk-intel-display/src/intel_cursor_full.rs` translates all 39
function definitions from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_cursor.c`
(MIT, © 2020 Intel), preserving cursor sizing, signed/PSR2 positioning,
surface-offset checks, 845/9xx generation policy, FBC/WM/SEL_FETCH write order,
legacy update cleanup, and cursor-plane creation policy. DRM clipping/FB
helpers, async/vblank-work lifetime, power, register/DSB access, and property
registration are explicit trait boundaries. The module is exported and has
focused position/configuration/WM-field tests. The live kernel plane and KMS
registration paths are not switched to this source module yet.
