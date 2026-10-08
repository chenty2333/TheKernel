# ADL-N shared-DPLL kernel adapter

`kernel/src/drm/intel/shared_dpll.rs` implements a typed kernel backend for
the translated `intel_dpll_mgr_full` source manager. It admits only known
ADL-N device/revision identities; maps combo DPLL0/1, TBT, and TC1/TC2 DKL
registers; uses the existing indexed DKL/HIP lock; and routes display power
references through `PowerState`. Unsupported register families, TC3/TC4,
unknown identity, unavailable MMIO, and unproven rollback fail closed. The
adapter retains per-device CRTC/DPLL state, reservation changes, reference
counts, warnings, and quarantine state.

The expected source-order integration is: verify identity, persist one
`SharedDpllState` for the DRM-device lifetime, initialize the manager, read all
active CRTC-to-DPLL mappings, then compute/reserve and swap atomic reservation
state before hardware enable/disable. An undo token may be restored only after
the outer display transaction has independently verified its hardware
rollback. Recreating the state for each modeset or sanitizing before complete
CRTC readout is invalid.

This adapter is compiled by `cargo check -p tk-kernel --tests --features
'intel-hda nvme watchdog-itco bpf'`; that command also type-checks its unit
tests but does not execute them. The current native path does **not** yet
instantiate persistent adapter state or dispatch modesets through it. In
particular, the fastboot TC path still uses its restricted transaction and
direct DKL sequence; its power-state lifetime does not yet provide the
refcounted domains required by this adapter. The module is a kernel integration
boundary, not evidence of active shared-DPLL operation on hardware.
