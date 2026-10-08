# i915 display 12/13 port map

Source baseline: Linux `v7.2.3`, `drivers/gpu/drm/i915/display/`. Scope is display versions 12/13 (TGL, RKL, ADL-S/P/N); device-specific tables and behavior must remain gated by the existing `DisplayDevice`/platform data. This is a working map and will be extended as modules are translated.

## Existing Rust layout and planned source mapping

| Work item | Linux source files | Existing Rust destination / integration |
|---|---|---|
| VBT and display power | `intel_bios.c`, `intel_display_power.c`, `intel_display_power_map.c`, `intel_display_power_well.c`, `intel_dmc.c` | `tk-intel-display::{intel_bios,opregion,device,dmc,power_map,power_well,dc_state}`; `kernel/src/drm/intel/{power,dmc,regs}`; DMC package parsing, TGL/RKL/ADLS/XELPD tables, HSW PW/fuse handshake and DC-state write retry are present; DMC event/MMIO load, interrupt-coupled well callbacks, map-driven full refcount/DC-state lifecycle remain |
| Clock / PLL | `intel_cdclk.c`, `intel_dpll_mgr.c`, `intel_dpll.c` | `tk-intel-display::{cdclk,dpll_mgr,dkl_phy}`; `kernel/src/drm/intel/{clk,pll,phy,regs}` |
| DDI / PHY / TC / HDMI | `intel_ddi.c`, `intel_ddi_buf_trans.c`, `intel_combo_phy.c`, `intel_tc.c`, `intel_hdmi.c` | `tk-intel-display::{ddi,tc,hdmi,device}`; `kernel/src/drm/intel/{output,tc_modeset,swing,phy,regs}` |
| DP / AUX / DDC / HPD | `intel_dp.c`, `intel_dp_link_training.c`, `intel_dp_aux.c`, `intel_gmbus.c`, `intel_hotplug.c`, `intel_hotplug_irq.c` | `tk-intel-display::{ddi,tc}` and new focused modules as needed; `kernel/src/drm/intel::{connect,gmbus,hpd,irq,sink,output}`; common I2C/DRM interfaces |
| Planes / scaler / watermarks / color / framebuffer / cursor / CRTC | `skl_universal_plane.c`, `skl_scaler.c`, `skl_watermark.c`, `intel_color.c`, `intel_fb.c`, `intel_cursor.c`, `intel_crtc.c`, `intel_vblank.c` | `tk-intel-display::{universal_plane,scaler,watermark,color,pipe_config}`; `kernel/src/drm/intel::{pipe,fb,scanout,timing,irq}`; `kernel/src/drm/{fbdev,kms,modes}` |
| Atomic modeset | `intel_display.c`, `intel_atomic.c`, `intel_modeset_setup.c`, `intel_modeset_verify.c` | `kernel/src/drm/intel::{modeset,fastboot,rollback,output,pipe}` and common `kernel/src/drm/{atomic,kms,property,screen}` |
| DRM userspace KMS | Linux `drm_atomic.c`, `drm_atomic_helper.c`, `drm_connector.c`, `drm_crtc.c`, `drm_plane.c`, `drm_property.c`, `drm_framebuffer.c` and related UAPI implementation | Common `kernel/src/drm/{atomic,device,file,ioctl,kms,property,screen,uapi,fbdev,modes}`; do not import DRM core wholesale |
| Optional display features (last) | DP subset of `intel_audio.c`, `intel_dp_mst.c`, `intel_psr.c`, `intel_fbc.c` | `tk-intel-display::audio` and new focused modules; kernel audio, connector, atomic and framebuffer interfaces |

## License gate

The selected i915 display files above are MIT-licensed. Files carrying `SPDX-License-Identifier: MIT` were checked directly; older files without SPDX were checked for the complete Intel MIT grant in their source headers. Keep all upstream copyright lines and include the full MIT text in the crate's `LICENSES/` directory before translating. The four GPL-2.0-only files in the upstream display directory are excluded from copying; their behavior may only be independently implemented from public specifications. Recheck the exact file header before each source translation, because directory membership is not a license grant.

Linux DRM common code is reviewed file-by-file under the same gate. Framework behavior is mapped onto TheKernel interfaces rather than importing Linux DRM core wholesale. Any translated function gets a `// upstream: <file> <function>()` locator comment.

## Boundaries / non-goals

This map does not include GT/GEM (`gt/`, `gtt/`, `gem_*.rs`, `gt_probe.rs`, owned by G2), legacy display generations, DSI/CRT/VGA, or generic Linux device frameworks. New support should preserve established TheKernel PCI/MMIO, DMA, interrupt, firmware, framebuffer and DRM abstractions. Additions to shared manifests, DRM generic files, and provenance/licensing registries are append-only.
