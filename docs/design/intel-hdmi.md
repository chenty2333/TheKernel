# Intel HDMI packet path

> **Status (2026-10-12, i915 unification phase 1): historical.** The native
> Intel display and GT stack this note describes (`kernel/src/drm/intel/`
> modesetting, fastboot, GTT, GEM and hotplug modules, and the
> `tk-intel-display` crate) has been removed. The Linux i915 port in
> `crates/ax/tk-intel-gt` is the only owner of the GPU, and the screen is the
> firmware (GOP) framebuffer console until the display code is ported. See
> `i915-gt-wiring-map.md` for the current wiring.

`crates/ax/tk-intel-display/src/hdmi.rs` translates two MIT `intel_hdmi.c` packet operations for the HSW/display-12/13 transcoder aperture: `intel_write_infoframe()`'s reserved/ECC hole insertion and `hsw_write_infoframe()`'s enable-clear, full 32-byte data writes, re-enable, and posting read. `gcp_default_phase_possible()`/`intel_hdmi_compute_gcp_infoframe()` calculate deep-color GCP phase, and `hsw_set_infoframes()` keeps the HSW GCP, AVI, SPD, vendor, DRM ordering. `intel_hdmi_fastset_infoframes()` clears and updates the DRM/HDR packet in place. The Intel SPD defaults and DRM metadata/version gates are also translated as caller-owned packet-state helpers. The HDMI/DP infoframe software-slot mapper is translated, and `apply_avi_output_policy()` implements the RGB/YCC range branch while leaving DRM colorimetry/content-type inputs to the connector layer. Display-12/13 source TMDS limits, Y420 half-clock scaling, source BPC limits, sink EDID deep-color gates, downstream TMDS limit ordering, display-12 PLL hole validation, and 12→10→8 BPC fallback are also translated. The typed readout path in the same module handles AVI/SPD/vendor/DRM status and GCP readback.

Full AVI/vendor/HDR packet construction and checksum validation from connector/EDID/atomic state, actual sink scrambling/SCDC negotiation, and full DVI/HDMI fastset wiring, dual-mode adapters, HDCP, CEC, LSPCON, audio policy, and other platform DIP engines remain untranslated or delegated to their owning workstreams. Packet writes require the caller to own powered pipe and output sequencing; this helper refuses unless the pipe is already powered.
