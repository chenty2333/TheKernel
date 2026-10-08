# Intel DDI helpers (display 12/13)

`crates/ax/tk-intel-display/src/ddi.rs` now contains source-shaped helpers from
Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_ddi.c` (MIT, Copyright © 2012
Intel): TBT/MG DDI clock selection/readback, DP link-rate and lane-stagger
fields, DDI buffer idle/active wait contracts, and the display-12/13
`TRANS_DDI_FUNC_CTL` builder for HDMI/DVI, DP SST/MST, UHBR and eDP input
fields. Tests pin the control values and wait polarity/timeout.

The transcoder/buffer helpers are portable; platform-specific DDI buffer
translation tables are in `ddi_buf_trans.rs`, including ADL-N HDMI defaults.
The combo DDI clock plan is consumed by `kernel/src/drm/intel/output.rs::program()`
through a locked kernel adapter, reached from the existing native modeset pipeline. `DdiBufferIo`
delegates register polls/delays to the platform; the kernel's typed register
set currently admits DDI buffer status only for ports A and B, so unsupported
TC/other ports refuse rather than synthesizing offsets. The current table module contains the ICL HDMI/eDP and TGL/RKL/ADLS/ADLP combo
and DKL DP/HDMI tables and source-shaped selection logic. Complete TC clock
routing, per-platform pre-enable/disable, connector setup, HDMI packets/SCDC,
DP AUX/link training, and the rest of `intel_ddi.c` remain untranslated. The
full MIT license is in `crates/ax/tk-intel-display/LICENSE-MIT`.
