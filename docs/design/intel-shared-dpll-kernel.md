# ADL-N shared-DPLL kernel adapter

`kernel/src/drm/intel/shared_dpll.rs` implements a typed kernel backend for
the translated `intel_dpll_mgr_full` source manager. It admits only known
ADL-N device/revision identities; maps combo DPLL0/1, TBT, and TC1/TC2 DKL
registers; uses the existing indexed DKL/HIP lock; and supports a
`PowerState` backend plus a read-only `PinnedDpllPower` context for the
firmware-preserving fastboot path. Unsupported register families, TC3/TC4,
unknown identity, unavailable MMIO, and unproven rollback fail closed. The
adapter retains CRTC/DPLL state, reservation changes, reference counts,
warnings, and quarantine state.

The expected source-order integration is: verify identity, persist one
`SharedDpllState` for the DRM-device lifetime, initialize the manager, read all
active CRTC-to-DPLL mappings, then compute/reserve and swap atomic reservation
state before hardware enable/disable. An undo token may be restored only after
the outer display transaction has independently verified its hardware
rollback. Recreating the state for each modeset or sanitizing before complete
CRTC readout is invalid.

Native fastboot now persists the manager for the KMS-device lifetime and uses
its generic DKL `get_hw_state` dispatcher for the selected TC1/TC2 PLL during
admission and before/after each restricted modeset, checking that its enable
bit agrees with the independent firmware capture. This is a read-only
live-path integration only: atomic reservation/commit still does not use the
manager, and `tc_modeset` owns the direct DKL enable/disable sequence. The pin
backend revalidates held source-mapped power requests, D0,
DC-state, and refclk on every hook/access; logical DPLL power cookies never
manufacture `PowerState` reference counts or change wells.

This adapter is compiled by `cargo check -p tk-kernel --tests --features
'intel-hda nvme watchdog-itco bpf'`; that command also type-checks its unit
tests but does not execute them. The focused test binary compiles but is not
currently linkable in the host test configuration because of existing
per-CPU `R_X86_64_32S` relocation errors. No native hardware operation has
been verified. The module still does not replace the restricted TC transaction
with the generic atomic manager lifecycle.
