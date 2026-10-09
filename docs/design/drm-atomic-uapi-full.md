# DRM atomic UAPI translation

`kernel::drm::atomic_uapi_full` translates all 30 function definitions in
Linux 7.2.3 `drivers/gpu/drm/drm_atomic_uapi.c` (MIT, 1,199 Rust lines). It
ports mode/blob/plane/connector/CRTC/color-operation property dispatch,
object-state lookup and references, user-array handling, async-flip checks,
out-fence setup/signaling, commit cleanup, and `EDEADLK` lock-backoff order.
DRM objects, usercopy, locks, blobs, fences, events and commit execution stay
behind `AtomicUapiIo`.

The module is declared and compile-checked in `tk-kernel`, but the active
`kernel/src/drm/ioctl.rs` and `atomic.rs` path still uses the existing custom
state implementation; an `AtomicUapiIo` adapter and state mapping remain
needed. Validation: worker standalone `rustc` passed, and primary
`cargo check -p tk-kernel --tests --features 'intel-hda nvme watchdog-itco bpf'`
passed after adapting the module's collections/format imports to `alloc`.
