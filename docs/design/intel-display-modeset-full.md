# Intel display modeset translation

`tk-intel-display::intel_display_modeset_full` translates 223 of the 273
ctags function definitions in Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_display.c` (MIT; 3,996 Rust lines).
Coverage is the display-12/13-relevant modeset path: atomic config/check and
retry, fastset predicates, HSW CRTC enable/disable ordering, joined pipes and
DDB, plane/flip state, CDCLK/mode limits, M/N, transcoder timing/readout,
encoder fanout and commit-tail/DSB sequencing. DRM/MMIO/firmware and
cross-subsystem operations are explicit hooks. The remaining 50 are listed
below by their display-generation or framework boundary; none are counted as
translated.

The following source functions are generation-specific outside the requested
display 12/13 target: `ilk_configure_cpu_transcoder`, `ilk_crtc_enable`,
`ilk_crtc_disable`, `i9xx_configure_cpu_transcoder`, `valleyview_crtc_enable`,
`i9xx_crtc_enable`, `i9xx_crtc_disable`, `i9xx_set_pipeconf`,
`i9xx_get_pipe_config`, `ilk_set_pipeconf`, `ilk_get_lanes_required`,
`ilk_get_pipe_config`, `ilk_pipe_pixel_rate`, `ilk_has_edp_a`,
`intel_crtc_supports_double_wide`, `hsw_mode_set_planes_workaround`,
`i830_enable_pipe`, `i830_disable_pipe`, `glk_need_scaler_clock_gating_wa`,
`glk_pipe_scaler_clock_gating_wa`, `skl_wa_827`, `needs_nv12_wa`,
`icl_wa_scalerclkgating`, `icl_wa_cursorclkgating`, `needs_scalerclk_wa`,
`needs_cursorclk_wa`, `intel_async_flip_vtd_wa`, `needs_async_flip_vtd_wa`,
and `intel_scanout_needs_vtd_wa` (their source gates are Gen9/11 or older).
`intel_phy_is_snps`, `intel_encoder_is_snps`, and
`intel_tc_phy_port_to_tc` are display-14+ PHY paths. `bxt_get_dsi_transcoder_state`
is the pre-display-12 DSI readout path; `intel_ddi_crt_present` and
`intel_encoder_destroy` are legacy-CRT or DRM object-lifecycle code.

Framework/debug-only boundaries not translated here are: `assert_transcoder`,
`assert_plane`, `assert_planes_disabled` (assert/log wrappers);
`intel_has_pending_fb_unpin` and `intel_display_flush_cleanup_work` (GEM/fb
cleanup scheduling); `pipe_config_mismatch`, `pipe_config_infoframe_mismatch`,
`pipe_config_dp_vsc_sdp_mismatch`, `pipe_config_dp_as_sdp_mismatch`,
`pipe_config_buffer_mismatch`, and `pipe_config_pll_mismatch` (diagnostic
diff/report helpers); and `transcoder_ddi_func_is_enabled`,
`hsw_enabled_transcoders`, `hsw_get_transcoder_state`,
`intel_panel_sanitize_ssc` (legacy-only readout/setup paths). These are not
needed for the target modeset algorithm; the target pipe configuration and
readout policy is represented by the translated mode-12/13 functions.

The module is exported and crate-tested, but the kernel's current N305 KMS
commit path has not yet been switched to this translated sequence; it is not
currently evidence of a cold-start hardware modeset.
