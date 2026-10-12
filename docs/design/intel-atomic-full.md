# Intel atomic modeset helper translation

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`tk-intel-display::intel_atomic_full` translates all 15 function definitions
from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_atomic.c` (MIT). It keeps
the connector property/fastset decisions, state-duplication reset list, blob
and tunnel reference ordering, DSB leak warnings, and allocation/clear/free
dispatch. DRM state/object ownership, HDCP and tunnel framework operations
are represented by `AtomicIo`.

The module is crate-tested but not yet wired into the kernel DRM atomic
callbacks; the existing KMS implementation remains the active path.
