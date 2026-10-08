# Intel DDI helpers (display 12/13)

`crates/ax/tk-intel-display/src/ddi.rs` now contains source-shaped helpers from
Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_ddi.c` (MIT, Copyright © 2012
Intel): TBT/MG DDI clock selection/readback, DP link-rate and lane-stagger
fields, DDI buffer idle/active wait contracts, and the display-12/13
`TRANS_DDI_FUNC_CTL` builder for HDMI/DVI, DP SST/MST, UHBR and eDP input
fields. Tests pin the control values and wait polarity/timeout.

The helpers are portable and tested but not yet called by a kernel modeset
sequence. `DdiBufferIo` delegates register polls/delays to the platform; the
kernel's typed register set currently admits DDI buffer status only for ports A
and B, so unsupported TC/other ports refuse rather than synthesizing offsets.
DDI clock mux writes, complete pre-enable/disable sequencing, connector setup,
HDMI packets/SCDC, DP AUX/link training, and the rest of `intel_ddi.c` remain
untranslated. The full MIT license is in `crates/ax/tk-intel-display/LICENSE-MIT`.
