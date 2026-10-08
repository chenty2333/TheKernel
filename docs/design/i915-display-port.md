# i915 display 12/13 port map

Source baseline: Linux `v7.2.3`, `drivers/gpu/drm/i915/display/`. Scope is display versions 12/13 (TGL, RKL, ADL-S/P/N); device-specific tables and behavior must remain gated by the existing `DisplayDevice`/platform data. This is a working map and will be extended as modules are translated.

## Existing Rust layout and planned source mapping

| Work item | Linux source files | Existing Rust destination / integration |
|---|---|---|
| VBT and display power | `intel_bios.c`, `intel_display_power.c`, `intel_display_power_map.c`, `intel_display_power_well.c`, `intel_dmc.c` | `tk-intel-display::{intel_bios,opregion,device,dmc,power_map,power_domains,power_well,dc_state}`; `kernel/src/drm/intel/{power,dmc,regs}`; DMC package/event fixups and validated main/pipe MMIO upload, TGL/RKL/ADLS/XELPD tables, display-12/13 TGL/RKL/ADLS/ADLP/N PCI-ID/stepping classifier, HSW PW/fuse handshake with ADL-P/N PG1 workaround gating, Pipe-A BIOS-to-driver well sync, synchronous/asynchronous domain reference logic, TGL/ICL TC-cold PCODE retry primitives, source-compatible domain names, per-HSW/DDI/AUX requester registers, descriptor-carried IRQ pipe masks, allowed/target DC policy, and tracked DC6 state setter are present; kernel PCI binding/modeset remains N305-specific, AUX TC PHY/reset/health paths, kernel TGL TC-cold map callback, live IRQ-coupled well callbacks (refused rather than skipped), kernel delayed-work/runtime-PM scheduling for async puts, full map-driven domain lifecycle, and DMC-controlled DC5/6/9 transitions remain |

`intel_bios.rs` now ports the review corrections for VBT defaults/field widths,
MIPI-v3 block sizing, LFP pointer fixup, panel-type/PnP selection, eDP/PSR
settings, DDC platform tables, and DDI child sanitization. Firmware, OpRegion,
SPI-ROM, and PCI-ROM VBT bytes are still provided by existing discovery
adapters; `intel_bios_get_vbt()` selects and validates supplied candidates.
The TheKernel result owns panel data, so i915's DRM-panel early/late/fini
allocation hooks map to one Rust init operation and ordinary ownership/drop.
The VBT export getter is present; debugfs file registration and log-only DDI
port printing are intentionally left to framework diagnostics, not copied into
the hardware-independent parser. `kernel/src/drm/intel/fastboot.rs` now obtains
the TC route and AFC override from `intel_bios_init()` rather than parsing those
fields through a separate partial path.
| Clock / PLL | `intel_cdclk.c`, `intel_dpll_mgr.c`, `intel_dpll.c` | `tk-intel-display::{intel_cdclk_full,cdclk,dpll,dpll_mgr,intel_dpll_mgr_full,dkl_phy}`; `kernel/src/drm/intel/{clk,pll,phy,regs}`. `intel_cdclk.c` translates 152/152 functions; `intel_dpll_mgr.c` translates 83/175, all display-12/13 plus generic i9xx no-manager fallbacks, with generation exclusions listed in `intel-dpll-manager-full.md`. Kernel has CDCLK transition predicates/MMIO adapter and ICL/TGL/DKL arithmetic and PLL adapters; active N305 output planning obtains default `Named` combo-PHY CFGCR words from translated `icl_calc_wrpll()`/`icl_calc_dpll_state()`. Atomic PCode/peripheral-lock CDCLK callsite, full `IntelDpllHooks` backend and active shared-DPLL lifecycle, MG PHY TGL/DP/TBT programming remain unconnected. |
| DDI / PHY / TC / HDMI | `intel_ddi.c`, `intel_ddi_buf_trans.c`, `intel_combo_phy.c`, `intel_tc.c`, `intel_hdmi.c` | `tk-intel-display::{ddi,tc,hdmi,device}`; `kernel/src/drm/intel/{output,tc_modeset,swing,phy,regs}`. `ddi.rs` has TBT/MG clock select/readback, DP buffer-link/rate/stagger fields, buffer wait contracts, a display-12/13 transcoder FUNC_CTL builder, and platform DPCLKA plan; `output.rs::program` consumes the locked combo DDI clock adapter. Full per-platform DDI/PHY/TC/HDMI pipeline still partial. |
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

Power-well translation also includes the BIOS-to-driver request handoff and
requester-order reads. The display-power subsystem remains partial: DDI/AUX,
IRQ-coupled well transitions, async puts, and full KMS reference lifetimes have
not yet been connected.

`PowerState` now exposes the map-backed synchronous get/put/is-enabled/get-if-
enabled API; the pipe-A boot path and a kernel unit path use it for AUX-A
references. `connect.rs`/`output.rs` still own direct AUX/DDI well sequences,
so the references are not yet unified across a complete modeset lifetime.
