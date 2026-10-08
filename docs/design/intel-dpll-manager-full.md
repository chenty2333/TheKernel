# i915 shared DPLL manager source translation

`crates/ax/tk-intel-display/src/intel_dpll_mgr_full.rs` and
`intel_dpll_mgr_remainder.rs` together translate Linux 7.2.3
`drivers/gpu/drm/i915/display/intel_dpll_mgr.c` (MIT, Copyright © 2006–2016
Intel). Ctags reports 175 functions: 83 are in the first module in exact
source order for display 12/13 and generic i9xx fallback, and the remainder
module adds the other 92 functions in its exact source order. The combined
coverage has one marker for every ctags function, with no omitted functions.
`IntelDpllHooks` and the generation-specific remainder hook traits carry atomic
DRM, platform selection, MMIO, power, and indexed PHY operations as explicit
backend dependencies.

Both modules are exported and compiled by `cargo check -p tk-intel-display
--lib`; the full crate suite previously passed 150 tests before the remainder
module was added. The active N305 HDMI planner consumes the translated
`icl_calc_wrpll()` and `icl_calc_dpll_state()` values. The kernel-side ADL-N
shared-DPLL adapter now compiles, but current atomic modeset does not yet
persist or call it; the fastboot TC power-state lifetime does not supply the
required refcounted port domains. Type-C/MG PHY runtime paths and
DP/Thunderbolt output call sites are also not connected. See
`intel-shared-dpll-kernel.md` for the required integration order and boundary.

## Added remainder grouped by generation

- BXT/Gen9 (14): `bxt_compare_hw_state`, `bxt_compute_dpll`,
  `bxt_ddi_dp_pll_dividers`, `bxt_ddi_dp_set_dpll_hw_state`,
  `bxt_ddi_hdmi_pll_dividers`, `bxt_ddi_hdmi_set_dpll_hw_state`,
  `bxt_ddi_pll_disable`, `bxt_ddi_pll_enable`, `bxt_ddi_pll_get_freq`,
  `bxt_ddi_pll_get_hw_state`, `bxt_ddi_set_dpll_hw_state`,
  `bxt_dump_hw_state`, `bxt_get_dpll`, `bxt_update_dpll_ref_clks`.
- HSW/Gen7–8 (26): `hsw_compare_hw_state`, `hsw_compute_dpll`,
  `hsw_ddi_calculate_wrpll`, `hsw_ddi_lcpll_compute_dpll`,
  `hsw_ddi_lcpll_disable`, `hsw_ddi_lcpll_enable`, `hsw_ddi_lcpll_get_dpll`,
  `hsw_ddi_lcpll_get_freq`, `hsw_ddi_lcpll_get_hw_state`,
  `hsw_ddi_spll_compute_dpll`, `hsw_ddi_spll_disable`, `hsw_ddi_spll_enable`,
  `hsw_ddi_spll_get_dpll`, `hsw_ddi_spll_get_freq`,
  `hsw_ddi_spll_get_hw_state`, `hsw_ddi_wrpll_compute_dpll`,
  `hsw_ddi_wrpll_disable`, `hsw_ddi_wrpll_enable`, `hsw_ddi_wrpll_get_dpll`,
  `hsw_ddi_wrpll_get_freq`, `hsw_ddi_wrpll_get_hw_state`,
  `hsw_dump_hw_state`, `hsw_get_dpll`, `hsw_update_dpll_ref_clks`,
  `hsw_wrpll_get_budget_for_freq`, `hsw_wrpll_update_rnp`.
- IBX/PCH-only (6): `ibx_assert_pch_refclk_enabled`, `ibx_compute_dpll`,
  `ibx_get_dpll`, `ibx_pch_dpll_disable`, `ibx_pch_dpll_enable`,
  `ibx_pch_dpll_get_hw_state`.
- SKL/Gen9 (21): `skl_compare_hw_state`, `skl_compute_dpll`,
  `skl_ddi_calculate_wrpll`, `skl_ddi_dp_set_dpll_hw_state`,
  `skl_ddi_dpll0_disable`, `skl_ddi_dpll0_enable`,
  `skl_ddi_dpll0_get_hw_state`, `skl_ddi_hdmi_pll_dividers`,
  `skl_ddi_lcpll_get_freq`, `skl_ddi_pll_disable`, `skl_ddi_pll_enable`,
  `skl_ddi_pll_get_freq`, `skl_ddi_pll_get_hw_state`,
  `skl_ddi_pll_write_ctrl1`, `skl_ddi_wrpll_get_freq`, `skl_dump_hw_state`,
  `skl_get_dpll`, `skl_update_dpll_ref_clks`, `skl_wrpll_get_multipliers`,
  `skl_wrpll_params_populate`, `skl_wrpll_try_divider`.
- MTL/display 14+ (16): `get_intel_encoder`, `mtl_compare_hw_state`,
  `mtl_compute_dplls`, `mtl_compute_non_tc_phy_dpll`,
  `mtl_compute_tc_phy_dplls`, `mtl_dump_hw_state`, `mtl_get_dplls`,
  `mtl_get_non_tc_phy_dpll`, `mtl_pll_disable`, `mtl_pll_enable`,
  `mtl_pll_get_freq`, `mtl_pll_get_hw_state`, `mtl_port_to_pll_id`,
  `mtl_tbt_pll_disable`, `mtl_tbt_pll_enable`, `mtl_tbt_pll_get_freq`.
- Xe3/display 35+ (9): `xe3plpd_compare_hw_state`, `xe3plpd_compute_dplls`,
  `xe3plpd_compute_non_tc_phy_dpll`, `xe3plpd_compute_tc_phy_dplls`,
  `xe3plpd_dump_hw_state`, `xe3plpd_pll_disable`, `xe3plpd_pll_enable`,
  `xe3plpd_pll_get_freq`, `xe3plpd_pll_get_hw_state`.

The generic i9xx dump/compare fallback functions in the first module are
included because DG2 uses them when no shared manager is installed; this is
source reachability, not a claim that DG2 is a target platform. All generation
groups above are translated through explicit backend traits, not evidence that
the kernel currently drives those generations.
