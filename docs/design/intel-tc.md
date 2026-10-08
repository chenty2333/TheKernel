# Type-C display helpers

`crates/ax/tk-intel-display/src/tc.rs` keeps portable Type-C port-state predicates, mode names, HPD-glitch policy, FIA-derived lane-count selection, and the existing ADL-P TC-DKL readout/signal-level helpers. The mode and lane functions follow Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_tc.c` and accept already-read state rather than owning MMIO or power references.

The kernel still lacks the full i915 TC port object/state machine: port mutex/refcount lifetime, runtime-PM/wakeref ownership, workqueue disconnect/link reset, legacy/DP-alt/TBT connect/disconnect, cold-block policy, TC PHY ops implementations for all platforms, DP/USB mux updates, pin assignment refresh and actual hotplug hooks are not translated/wired. The current DKL output path remains limited to its already-owned ADL-N firmware route.
