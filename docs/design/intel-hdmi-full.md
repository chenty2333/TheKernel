# Intel HDMI source translation

`crates/ax/tk-intel-display/src/intel_hdmi_full.rs` translates 114/120
function definitions in Linux 7.2.3 `drivers/gpu/drm/i915/display/intel_hdmi.c`
(MIT, Dave Airlie 2006 and Intel 2006–2009). It contains the source-ordered
infoframe, GCP, TMDS/BPC/format, mode-validation, dual-mode, DDC/SCDC, HDCP,
FRL and DSC decisions; hardware, DDC and DRM framework work is represented by
narrow traits. Omitted definitions are `intel_hdmi_add_properties`,
`intel_hdmi_connector_atomic_check`, `intel_hdmi_connector_register`,
`intel_hdmi_connector_unregister`, `intel_hdmi_get_modes`, and
`intel_infoframe_init`, which are generic DRM connector/property/modes or
registration wrappers. The full MIT grant is preserved in the module and
`LICENSE-MIT`. The module is exported and compiles with the crate. The native
ADL-N TC modeset now calls the translated `intel_hdmi_compute_clock()` before
its first destructive write, using the implemented RGB/8-bpc/no-scrambling
limits to reject rates outside 25–300 MHz and the lowest nonzero HDMI VSDB
TMDS limit from CTA EDID. Missing max-clock fields remain unknown rather than
being inferred. The same helper now hides the optional 1080p60 KMS mode when
the sink's advertised TMDS limit is too low; the active firmware mode remains
published so boot state is not silently changed. This is a narrow admission
hook, not a complete HDMI backend: the infoframe,
SCDC, HDCP, FRL, connector, and DSC pipelines are still not live in the kernel
path.
