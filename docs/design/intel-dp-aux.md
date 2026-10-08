# Intel DP AUX transaction engine

`crates/ax/tk-intel-display/src/dp_aux.rs` is a source-ordered translation of
all 35 function definitions (929 upstream lines) in Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dp_aux.c` (MIT, Copyright © 2020-2021
Intel Corporation). The 1,000-line Rust module keeps the AUX header and
big-endian register packing, generation/channel register selection, clock and
send-control formulas, HDCP AKSV flag, timeout/error/retry behavior, message
semantics, channel conflict selection, and power/PPS/QoS cleanup order.

The portable module maps DRM AUX, MMIO, power references, PPS, CPU-latency QoS,
interrupt waits, and diagnostics to `DpAuxIo`. `kernel/src/drm/intel/dp_aux.rs`
implements a typed A/B register and `PowerState` adapter, with bounded
poll-based completion for the current N305 path. The native `connect` phase
now probes the 16-byte DPCD base-capability block after a live GMBUS/HPD A/B
result and records either revision or failure. That DPCD snapshot is
diagnostic/admission input only: DP bandwidth selection and link training are
not yet called by the modeset. The adapter is external-DP only: PPS/eDP methods
are placeholders, CPU-latency QoS has no x86 platform interface, and source
async power put maps to a synchronous balanced release because no delayed put
workqueue is wired. The former hand-written GMBUS implementation is not
evidence of AUX transport.
`kernel/src/drm/intel/regs/aux.rs` declares the A/B channel-control and five
data dwords from the MIT `intel_dp_aux_regs.h` table. The rollback transaction
classifies these AUX command/data registers as transient and leaves restoration
to the AUX state machine's status clearing.
The upstream debugfs-independent AUX helpers are translated; framework calls
are represented by the adapter rather than importing DRM AUX infrastructure.

The MIT text is `crates/ax/tk-intel-display/LICENSE-MIT`.
