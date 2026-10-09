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
implements typed AUX A/B and Type-C TC1/TC2 (source AUX D/E) register mappings,
separate channel locks, and `PowerState` domain references, with bounded
poll-based completion. Its TC path clears `DP_AUX_CH_CTL_TBT_IO` before asking
the map-backed `AUX_USBC1/2` wells for power. The native `connect` phase
now probes the 16-byte DPCD base-capability block through the
`intel_dp_full::DpAuxIo` adapter after a live GMBUS/HPD A/B result and records
either revision or failure. That DPCD snapshot is
diagnostic/admission input only: neither Type-C DPCD probe nor DP bandwidth
selection/link training is yet called by the modeset. The adapter is external-DP
only: PPS/eDP methods
are placeholders, CPU-latency QoS has no x86 platform interface, and source
async power put maps to a synchronous balanced release because no delayed put
workqueue is wired. The former hand-written GMBUS implementation is not
evidence of AUX transport.
`kernel/src/drm/intel/regs/aux.rs` declares the A/B and D/E channel-control and
five data dwords from the MIT `intel_dp_aux_regs.h` table. In the upstream
`intel_display_limits.h`, `AUX_CH_USBC1` aliases `AUX_CH_D` and `USBC2` aliases
`AUX_CH_E`; `_PICK_EVEN` applies the 0x100 per-channel stride. The rollback transaction
classifies these AUX command/data registers as transient and leaves restoration
to the AUX state machine's status clearing.
This MMIO mapping is separate from the DKL PHY lane signal-level registers.
Adding AUX D/E transport and power-well support does not itself enable a
TC1/TC2 DP-alt link: connector/VBT route selection, DPCD negotiation,
link-training state, full mode rollback, and DKL PHY lane programming are
still not connected. The HSW well adapter also does not yet reproduce i915's
post-request DKL uC-health warning poll.
The upstream debugfs-independent AUX helpers are translated; framework calls
are represented by the adapter rather than importing DRM AUX infrastructure.

The MIT text is `crates/ax/tk-intel-display/LICENSE-MIT`.
