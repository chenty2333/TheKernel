# Fixed-mode firmware KMS

**未在硬件上验证.** `drm/linear.rs` presents RAM dumb buffers to the firmware's
already-scanning aperture. It offers one connector/encoder/CRTC/primary plane,
no hardware cursor or render node, and only the boot framebuffer's geometry.
The 60 Hz event cadence is software timing, **not** a measured firmware refresh
rate. No GOP, PLL, PHY, pipe, power-well or display-disable register is written.

The source is validated and all GEM page addresses are resolved before the
first screen write. The destination uses its real pitch and channel layout.
Unsupported modes are rejected even during TEST_ONLY. Copying can tear and is
not zero-copy acceleration; the goal is a working CPU-rendered KMS fallback.
Recoverable mapping/backend/dumb-allocation errors leave the direct firmware
framebuffer available as a console candidate. The shared DRM core still uses
infallible initialization allocations; this is not global OOM recovery. Blanking an inactive KMS state does not disable
the physical output.

A native Intel surface may register the same copy adapter **only** after the
existing scanout gate confirms advancing PIPEDSL, the correct SURFLIVE address
and a non-idle output. It then precedes firmware KMS. However, the existing
Intel modeset has no complete restoration of the firmware's active state on
failure. A boot with a valid firmware framebuffer therefore stops after the
read-only Intel probe, before power/modeset writes, and logs the reason. Native
1080p modesetting and a full Linux 7.2.3 register-order audit remain incomplete.
Keeping an aperture address alone was not a valid rollback guarantee.

The fallback publishes an actual platform parent and modalias for libdrm;
it does not impersonate a VirtIO PCI device. Known unsupported async page flip
returns capability value zero, as SDL KMSDRM expects, rather than EINVAL.

`--gfxmode 1920x1080x32,auto` in the PXE tool asks GRUB to set a GOP mode;
there is deliberately no `gfxpayload=keep`. Whether the target GOP advertises
that mode, and its real refresh, must be checked tonight. Unavailable modes
fall back to automatic selection without claiming 1080p success.

Reference behavior: Linux `drivers/gpu/drm/sysfb/efidrm.c`,
`drm_sysfb_modeset.c` and `simpledrm.c` (fixed firmware mode plus shadow copy).
The Rust adapter is original; no Linux code or firmware was copied into it.
Validation outcomes and tonight's commands are in `n305-tonight.md`.
