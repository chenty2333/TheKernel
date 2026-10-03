# Firmware display preservation: fail-closed preparation (2026-10-04)

**B3 is not complete. Native modesetting is NOT enabled, even with
`intel.modeset=1`. 未在硬件上验证.** A fake register window cannot establish that
firmware scanout survives a failed PLL/PHY transition on the N305.

## Implemented boundary

Default boot stops before *all* display programming, including when there is
no GOP framebuffer tag. Explicit `intel.modeset=1` requests a read-only
candidate snapshot of the existing driver's known register inventory: display
power/clock, combo PHY A/B, DPLL, DDI, transcoder, pipe, plane and watermark
registers. Offset deduplication preserves one initial value per register;
missing reads are retained as unavailable, never converted to zero. Debugfs
reports the candidate and that it does not permit destructive programming.

A candidate is **not** a complete firmware image: powered-down banks can read
zero, and the driver's table is not an inventory of every ADL-P/N firmware
register. No real restoration API is exposed. The old modeset is still
refused. Host tests use the existing fake registers for read-only capture,
missing-bank refusal and a RAM-register undo model. The latter checks every
failure prefix and exact reverse write/content restoration; it deliberately
does not pretend to implement hardware PLL/PHY/power sequencing.

## Why replaying saved dwords is not enough

Linux 7.2.3 `drivers/gpu/drm/i915/display/` was used to inspect the dependency
order (`intel_display_power_map.c`, `intel_cdclk.c`, `intel_combo_phy.c`,
`intel_dpll_mgr.c`, `intel_ddi.c`, `intel_crtc.c`, `skl_universal_plane.c`,
`skl_watermark.c`, `intel_modeset_setup.c`). ADL-N is an ADL-P subplatform.
Firmware state must include all enabled pipes/planes/transcoders, ports,
clock routing, port PLLs, combo/Type-C PHY state, CDCLK, DBUF/DDB/watermarks,
power requesters/DC policy and the GGTT entries used by firmware and the
attempted new scanout. Retain the firmware framebuffer memory and geometry.

A rollback implementation must classify RW versus RO/status/W1C/self-clearing
and masked-write registers; an enable's readback contains status bits that
must not be blindly written back. Shared PLLs and power wells may serve an
unmodified pipe. Type-C ownership/PHY access is not interchangeable with combo
PHY. GGTT PTEs and new framebuffer DMA owners need an explicit undo/quarantine
contract, not just register replay. The snapshot must precede the first power,
clock, hotplug/GMBUS or modeset write, not merely precede `set_mode()`.

Restoration needs bounded disable/readback steps for the attempted plane,
pipe/transcoder and DDI, then dependency-aware undo of clock/PLL/PHY changes,
then firmware route/timing/watermark/PTE/plane reinstatement, and finally power
requests/DC policy. The order of *operations* must reverse the attempted
sequence, but hardware prerequisites constrain the order of each operation's
internal writes. Report both the original failure and any rollback failure.
Never report "firmware console retained" merely because an address is saved.

## Required scanout acceptance before unlocking modeset

After rollback, check the firmware pipe/transcoder/plane and live surface,
restore the exact relevant PTEs, and sample advancing scanline/frame counters
with a bounded deadline. Register equality alone is insufficient; capture the
physical output and compare a known framebuffer pattern/console update. A
counter alone also cannot prove the monitor has the restored picture. Require
both progression and visible firmware framebuffer output, including the
original pitch/pixel layout. Failed restoration must not free DMA still being
scanned or continue to another modeset.

For the eventual opt-in native path, obtain EDID through the actual port/VBT
route; choose the advertised mode, then test 1920x1080@60 if present. The saved
capture-dongle EDID is not a substitute for a live monitor EDID. Test injected
failure after each phase, not just successful 1080p. Until the above contracts
exist, the only safe result of the parameter is a diagnostic refusal.
