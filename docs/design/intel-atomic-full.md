# Intel atomic modeset helper translation

`tk-intel-display::intel_atomic_full` translates all 15 function definitions
from Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_atomic.c` (MIT). It keeps
the connector property/fastset decisions, state-duplication reset list, blob
and tunnel reference ordering, DSB leak warnings, and allocation/clear/free
dispatch. DRM state/object ownership, HDCP and tunnel framework operations
are represented by `AtomicIo`.

The module is crate-tested but not yet wired into the kernel DRM atomic
callbacks; the existing KMS implementation remains the active path.
