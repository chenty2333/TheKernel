# Native Intel atomic-modeset projection

`kernel/src/drm/intel/atomic_modeset_wiring.rs` builds a narrow translated
`AtomicState` projection from the current Native mode-change request and invokes
only pure source checks whose inputs are fully modeled: digital-port conflict,
PHY-to-Type-C mapping, and baseline pipe bpp. It admits Pipe A, TC1/TC2, RGB
8-bpc, one linear XRGB8888 primary plane, and canonical VIC 16/95 timings.
Unsupported source actions are classified as owned by the indivisible
`tc_modeset::program` transaction or refused; the classifier never converts a
missing callback into success.

This projection is wired before the actual fastboot mode-change transaction and
is validated by `cargo check -p tk-kernel --tests --features 'intel-hda nvme
watchdog-itco bpf'`. It is only an admission preflight. It does not call
`intel_crtc_atomic_check`, `intel_atomic_check`, `hsw_crtc_enable`,
`hsw_crtc_disable`, or `intel_atomic_commit_tail`, and it does not perform
register writes. The old transactional TC path remains the only writer. Full
atomic orchestration still needs exact callback implementations for DMC,
CDCLK, shared DPLL, DDI/DP training, color, WM/DBUF, DSB, and rollback before
that path can safely be replaced.
