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

Native fastboot now persists the manager for the KMS-device lifetime. At
admission it limits the manager view to DPLL0/1, TBT and the one TC DKL PLL
whose DDI/AUX wells the `PowerPin` proves (TC2's DPLL4 is moved to the active
slot and the unpowered sibling is excluded), then calls source
`intel_dpll_readout_hw_state()` with the captured Pipe-A CRTC owner. The public
generic DKL `get_hw_state` dispatcher still independently checks the selected
PLL before/after each restricted modeset, comparing its enable bit and
source-comparable masked DKL register fields with the firmware capture via
translated `icl_compare_hw_state`. The adapter reconstructs Pipe-A's MG port
reservation from that readout and uses translated source compute/release/
reserve at initial readout. On later admitted TC mode changes, the manager
plans and swaps the selected port's default-TBT and MG-DKL reservations. Its
source DKL disable/enable callbacks now own the active PLL off/program/lock
sequence inside the existing transaction, with a software undo token restored
only after the outer rollback independently verifies the old hardware image.
The rest of `tc_modeset` remains the bounded TC HDMI transaction; this does not
wire the generic HSW CRTC/atomic commit-tail lifecycle. The pin backend
revalidates held source-mapped power requests, D0, DC-state, and refclk on every
hook/access; logical DPLL power cookies never manufacture `PowerState`
reference counts or change wells.

Before manager initialization, fastboot now carries the parsed VBT AFC-startup
override (including the distinction between no override and an explicit zero)
into `IntelDpllDisplay::vbt`; the source DKL writer uses the same override on
the active TC transaction.

For each admitted TC HDMI mode transition, fastboot calls translated
`intel_dpll_compute()` and `intel_dpll_reserve()` for the selected Pipe-A/TC
encoder and compares the source-computed DKL fields with the transaction's
planned DKL image before any display write. A mismatch refuses the transition.
The host source test compares manager calculations against the standalone DKL
planner at 148.5 and 297 MHz, including an explicit AFC startup override; it
passes. Kernel test code is compile-checked but the bare-metal host linker
cannot execute its per-CPU test image.
The source crate has a model test that compares the manager computation to the
standalone DKL planner at 148.5 and 297 MHz, including an explicit AFC startup
override; it passes on the host test target.

The scoped manager now remembers which TC port was selected and checks
allocator/CRTC arguments against it before entering source compute/reserve/
release or DKL enable/disable hooks. It rejects sibling TC selection, legacy
C/D aliases that do not match the selected DKL PLL, and invalid CRTC indices.
This guard is used by the active selected-port modeset path and prevents the
single-port `PowerPin` from being reinterpreted as power for its hidden sibling.

The source manager's unrestricted all-PLL readout is not called with this
single-port pin: its TC1/TC2 DKL enumeration would touch the unpowered sibling.
The source manager is instead scoped to the one power-proven DKL PLL plus
non-TC manager entries that use display-core MMIO. This scoping is an adapter
policy, not a claim that the unpowered route is inactive; an allocator shared
with a second active TC route still requires a broader verified power context.

This adapter is compiled by `cargo check -p tk-kernel --tests --features
'intel-hda nvme watchdog-itco bpf'`; that command also type-checks its unit
tests but does not execute them. The focused test binary compiles but is not
currently linkable in the host test configuration because of existing
per-CPU `R_X86_64_32S` relocation errors. No native hardware operation has
been verified. The module still does not replace the restricted TC transaction
with the generic HSW/atomic commit-tail lifecycle.
