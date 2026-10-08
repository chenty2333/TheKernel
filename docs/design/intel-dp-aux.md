# Intel DP AUX transaction engine

`crates/ax/tk-intel-display/src/dp_aux.rs` is a source-ordered translation of
all 35 function definitions (929 upstream lines) in Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dp_aux.c` (MIT, Copyright © 2020-2021
Intel Corporation). The 1,000-line Rust module keeps the AUX header and
big-endian register packing, generation/channel register selection, clock and
send-control formulas, HDCP AKSV flag, timeout/error/retry behavior, message
semantics, channel conflict selection, and power/PPS/QoS cleanup order.

The portable module maps DRM AUX, MMIO, power references, PPS, CPU-latency QoS,
interrupt waits, and diagnostics to `DpAuxIo`. There is not yet a kernel
`DpAuxIo` implementation connected to the N305 connector probe / modeset path;
the former hand-written GMBUS implementation is not evidence of an AUX transport.
Consequently DP link training and DPCD-dependent output selection must remain
refused until the typed register adapter and the port lifecycle are wired.
`kernel/src/drm/intel/regs/aux.rs` now declares the A/B channel-control and five
data dwords from the MIT `intel_dp_aux_regs.h` table, but these entries do not
yet constitute a `DpAuxIo` backend or authorize a caller to start AUX traffic.
The upstream debugfs-independent AUX helpers are translated; framework calls
are represented by the adapter rather than importing DRM AUX infrastructure.

The MIT text is `crates/ax/tk-intel-display/LICENSE-MIT`.
