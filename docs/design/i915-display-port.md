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
| Clock / PLL | `intel_cdclk.c`, `intel_dpll_mgr.c`, `intel_dpll.c` | `tk-intel-display::{intel_cdclk_full,cdclk,dpll,dpll_mgr,intel_dpll_mgr_full,dkl_phy}`; `kernel/src/drm/intel/{clk,pll,phy,regs}`. `intel_cdclk.c` and `intel_dpll_mgr.c` translate 152/152 and 175/175 functions respectively, with generation exclusions listed in `intel-dpll-manager-full.md`. Kernel has CDCLK transition predicates/MMIO adapter; the opt-in N305 boot transaction raises CDCLK only with all pipes and combo DDI link disabled; rollback restores clock and PCODE voltage. Native TC modesets now have a raise-only CDCLK callback: it requires all PIPECONF, transcoder FUNC_CTL and every DDI_BUF_CTL to read disabled while the transaction is quiesced; unreadable peers refuse. Generic active-pipe CDCLK and lowering remain unconnected. ICL/TGL/DKL arithmetic and PLL adapters are present; active N305 output planning obtains default `Named` combo-PHY CFGCR words from translated `icl_calc_wrpll()`/`icl_calc_dpll_state()`. Fastboot reconstructs selected-port TBT/MG reservations, performs translated compute/release/reserve/swap, and uses source DKL enable/disable/lock callbacks in its bounded TC transaction with verified software-state rollback. The adapter remains limited to one power-proven TC1/2 port; DP/TBT/other-port PLL lifecycles remain unconnected. |
| DDI / PHY / TC / HDMI | `intel_ddi.c`, `intel_ddi_buf_trans.c`, `intel_combo_phy.c`, `intel_tc.c`, `intel_hdmi.c` | `tk-intel-display::{ddi,tc,hdmi,device}`; `kernel/src/drm/intel/{output,tc_modeset,swing,phy,regs}`. `ddi.rs` has TBT/MG clock select/readback, DP buffer-link/rate/stagger fields, buffer wait contracts, a display-12/13 transcoder FUNC_CTL builder, and platform DPCLKA plan; `output.rs::program` consumes the locked combo DDI clock adapter. Active N305 TC1/2 HDMI programming now calls translated transcoder clock enable/disable, FUNC_CTL/CTL2, DDI_BUF_CTL enable/disable, HSW AVI writer, and shared-DPLL DKL disable/enable helpers through bounded adapters with readbacks; DKL signal levels and TC ownership remain in the constrained transaction. Full per-platform DDI/PHY/TC/HDMI pipeline still partial. |
| DP / AUX / DDC / HPD | `intel_dp.c`, `intel_dp_link_training.c`, `intel_dp_aux.c`, `intel_gmbus.c`, `intel_hotplug.c`, `intel_hotplug_irq.c` | `tk-intel-display::{ddi,tc}` and new focused modules as needed; `kernel/src/drm/intel::{connect,gmbus,hpd,irq,sink,output}`; common I2C/DRM interfaces |
| Planes / scaler / watermarks / color / framebuffer / cursor / CRTC | `skl_universal_plane.c`, `skl_scaler.c`, `skl_watermark.c`, `intel_color.c`, `intel_fb.c`, `intel_cursor.c`, `intel_crtc.c`, `intel_vblank.c` | `tk-intel-display::{universal_plane,scaler,watermark,color,pipe_config}`; `kernel/src/drm/intel::{pipe,fb,scanout,timing,irq}`; `kernel/src/drm/{fbdev,kms,modes}`. Native advertises linear XR24 and RG16/RGB565; CREATE_DUMB, legacy ADDFB, and ADDFB2 can construct those 16-bpp buffers, matching the existing pipe planner/readout path. `pipe.rs::plan_multi_plane_dbuf()` now provides a pure, conservative packed-RGB DBUF/watermark planner, but it is not called by Native or KMS; hardware multi-plane programming therefore remains unsupported. Cursor and multi-CRTC DBUF programming remain unsupported. DisplayAdapter reports generation-specific gamma/degamma LUT sizes, with ADL-N degamma=131 per source. Native gamma/CTM/degamma writes remain refused because KMS does not deliver color blob payloads to the hardware adapter and the full color before-image/rollback path is absent. |
| Atomic modeset | `intel_display.c`, `intel_atomic.c`, `intel_modeset_setup.c`, `intel_modeset_verify.c` | `kernel/src/drm/intel::{modeset,fastboot,rollback,output,pipe}` and common `kernel/src/drm/{atomic,kms,property,screen}`. N305 fastboot uses source mode validation/projection, shared-DPLL transaction state, and translated CRTC verification around its legacy TC HDMI transaction. Generic HSW CRTC enable/disable and `intel_atomic_commit_tail` still do not drive MMIO, so the constrained transaction has not been replaced. |
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

## Native atomic activation replacement plan (2026-10-09)

The active userspace path is `DRM_IOCTL_MODE_ATOMIC` in `kernel/src/drm/ioctl.rs::atomic()` → `DrmFile::submit_atomic()` → `DrmDevice` job queue / `advance_atomic_commit()` → `complete_atomic()` in `kernel/src/drm/device.rs` → `DisplayAdapter::present()` → `Native::present()` in `kernel/src/drm/intel/fastboot.rs`. Today the Native adapter validates and prepares the framebuffer, then `tc_modeset::program()` owns one monolithic TC1/2 HDMI transaction; the translated `intel_atomic_commit_tail()` is not the hardware path.

Replacement will proceed behind this queue boundary, maintaining one serialized transaction and the existing before-image/readback/rollback/quarantine guarantees:

1. Move the existing quiesced CDCLK raise/restore callback behind the translated `SetCdclkPrePlaneUpdate`/`SetCdclkPostPlaneUpdate` operations; keep refusing a transition until all consumers are proven disabled.
2. Feed the translated DPLL atomic state the validated target clock/route, perform source-shaped get/reserve/swap and source enable/disable callbacks, and include allocator state in rollback.
3. Replace TC's direct phase ownership incrementally with translated DDI/TC output hooks. Keep cold-exit, ownership, AUX/DP training and failures fail-closed until each has checked power references, bounded waits, readback and rollback.
4. Move pipe/plane programming to translated CRTC and universal-plane steps; derive and verify WM/DDB before any arm, with vblank/DMA-retirement ownership retained.
5. Acquire/release map-backed power domains around those phases, then enable DC transitions only with their observers and delayed puts. Run the translated commit tail only when every action emitted for this supported state has a real adapter; remove the matching legacy phase in the same change.

Each phase is a separate commit with focused host/kernel compile checks. The old TC transaction remains the rollback owner until an equivalent translated sequence passes full register-image and failure-injection tests; no preflight or no-op callback counts as activation. Current phase status is tracked in `progress-G1.md`.

### Opt-in selection, error handling, and N305 validation

**Implementation status: the selector and generic Native `ModesetOps` adapter
are not implemented yet.** Thus `intel.native_modeset=1` is currently only the
agreed future switch, not a working feature. With no selector present, Native
continues to use the current TC transaction and its rollback, regardless of
that parameter. The existing top-level `intel.modeset=1` write opt-in remains
required. Once implemented, the new path must not automatically retry through
the old transaction: an error follows the translated commit's cleanup/unwind
path and then fails the commit. If cleanup cannot prove
that pipe, plane, PLL, CDCLK, DDI/TC and power-domain state are restored, the
adapter must mark the device lost and retain/quarantine every possibly scanned
out DMA binding. It must not start a second writer against uncertain hardware.
The new path stays opt-in until physical N305 validation is complete.

Before changing the eventual default, test on the actual N305 with the Fedora
fallback kernel and remote/serial recovery available. Keep Secure Boot and the
known-good boot entry intact. First boot with `intel.modeset=1 intel.native_modeset=1`
on the verified TC1/TC2 HDMI topology; retain the full early-kernel log and
confirm the selected path explicitly reported itself. Verify firmware scanout
readout before any atomic commit, then exercise one initial modeset, framebuffer
flip, disable/re-enable, and a supported mode change. For every operation,
verify KMS completion, DDI/transcoder/PLL/CDCLK/WM readbacks, vblank progression,
no underrun, and correct pixels; inject failures at each phase and verify
rollback or device-loss quarantine. Also exercise the old path without
`intel.native_modeset=1`, then reboot the fallback kernel and confirm the
firmware console remains recoverable. Do not make the new path default on QEMU,
compile-only or model evidence.
