// SPDX-License-Identifier: MIT
// Copyright © 2016 Intel Corporation.
// PCI device-ID lists from Linux v7.2.3 include/drm/intel/pciids.h, expanded
// in the order used by intel_device_info.c (subplatform_*_ids) and i915_pci.c
// (pciidlist). The complete MIT grant is retained in ../LICENSE-MIT.

#![allow(dead_code, non_upper_case_globals)]

/// subplatform list `subplatform_ult_ids` (intel_device_info.c), from INTEL_HSW_ULT_GT1_IDS, INTEL_HSW_ULT_GT2_IDS, INTEL_HSW_ULT_GT3_IDS, INTEL_BDW_ULT_GT1_IDS, INTEL_BDW_ULT_GT2_IDS, INTEL_BDW_ULT_GT3_IDS, INTEL_BDW_ULT_RSVD_IDS, INTEL_SKL_ULT_GT1_IDS, INTEL_SKL_ULT_GT2_IDS, INTEL_SKL_ULT_GT3_IDS, INTEL_KBL_ULT_GT1_IDS, INTEL_KBL_ULT_GT2_IDS, INTEL_KBL_ULT_GT3_IDS, INTEL_CFL_U_GT2_IDS, INTEL_CFL_U_GT3_IDS, INTEL_WHL_U_GT1_IDS, INTEL_WHL_U_GT2_IDS, INTEL_WHL_U_GT3_IDS, INTEL_CML_U_GT1_IDS, INTEL_CML_U_GT2_IDS.
pub const SUBPLATFORM_ULT_IDS: &[u16] = &[0x0A02, 0x0A06, 0x0A0A, 0x0A0B, 0x0A12, 0x0A16, 0x0A1A, 0x0A1B, 0x0A22, 0x0A26, 0x0A2A, 0x0A2B, 0x0A2E, 0x1606, 0x160B, 0x1616, 0x161B, 0x1626, 0x162B, 0x1636, 0x163B, 0x1906, 0x1913, 0x1916, 0x1921, 0x1923, 0x1926, 0x1927, 0x5906, 0x5913, 0x5916, 0x5921, 0x5926, 0x3EA9, 0x3EA5, 0x3EA6, 0x3EA7, 0x3EA8, 0x3EA1, 0x3EA4, 0x3EA0, 0x3EA3, 0x3EA2, 0x9B21, 0x9BAA, 0x9BAC, 0x9B41, 0x9BCA, 0x9BCC];

/// subplatform list `subplatform_ulx_ids` (intel_device_info.c), from INTEL_HSW_ULX_GT1_IDS, INTEL_HSW_ULX_GT2_IDS, INTEL_BDW_ULX_GT1_IDS, INTEL_BDW_ULX_GT2_IDS, INTEL_BDW_ULX_GT3_IDS, INTEL_BDW_ULX_RSVD_IDS, INTEL_SKL_ULX_GT1_IDS, INTEL_SKL_ULX_GT2_IDS, INTEL_KBL_ULX_GT1_IDS, INTEL_KBL_ULX_GT2_IDS, INTEL_AML_KBL_GT2_IDS, INTEL_AML_CFL_GT2_IDS.
pub const SUBPLATFORM_ULX_IDS: &[u16] = &[0x0A0E, 0x0A1E, 0x160E, 0x161E, 0x162E, 0x163E, 0x190E, 0x1915, 0x191E, 0x590E, 0x5915, 0x591E, 0x591C, 0x87C0, 0x87CA];

/// subplatform list `subplatform_portf_ids` (intel_device_info.c), from INTEL_ICL_PORT_F_IDS.
pub const SUBPLATFORM_PORTF_IDS: &[u16] = &[0x8A50, 0x8A52, 0x8A53, 0x8A54, 0x8A56, 0x8A57, 0x8A58, 0x8A59, 0x8A5A, 0x8A5B, 0x8A5C, 0x8A70, 0x8A71];

/// subplatform list `subplatform_uy_ids` (intel_device_info.c), from INTEL_TGL_GT2_IDS.
pub const SUBPLATFORM_UY_IDS: &[u16] = &[0x9A40, 0x9A49, 0x9A59, 0x9A78, 0x9AC0, 0x9AC9, 0x9AD9, 0x9AF8];

/// subplatform list `subplatform_n_ids` (intel_device_info.c), from INTEL_ADLN_IDS.
pub const SUBPLATFORM_N_IDS: &[u16] = &[0x46D0, 0x46D1, 0x46D2, 0x46D3, 0x46D4];

/// subplatform list `subplatform_rpl_ids` (intel_device_info.c), from INTEL_RPLS_IDS, INTEL_RPLU_IDS, INTEL_RPLP_IDS.
pub const SUBPLATFORM_RPL_IDS: &[u16] = &[0xA780, 0xA781, 0xA782, 0xA783, 0xA788, 0xA789, 0xA78A, 0xA78B, 0xA721, 0xA7A1, 0xA7A9, 0xA7AC, 0xA7AD, 0xA720, 0xA7A0, 0xA7A8, 0xA7AA, 0xA7AB];

/// subplatform list `subplatform_rplu_ids` (intel_device_info.c), from INTEL_RPLU_IDS.
pub const SUBPLATFORM_RPLU_IDS: &[u16] = &[0xA721, 0xA7A1, 0xA7A9, 0xA7AC, 0xA7AD];

/// subplatform list `subplatform_g10_ids` (intel_device_info.c), from INTEL_DG2_G10_IDS, INTEL_ATS_M150_IDS.
pub const SUBPLATFORM_G10_IDS: &[u16] = &[0x56A0, 0x56A1, 0x56A2, 0x56BE, 0x56BF, 0x5690, 0x5691, 0x5692, 0x56C0, 0x56C2];

/// subplatform list `subplatform_g11_ids` (intel_device_info.c), from INTEL_DG2_G11_IDS, INTEL_ATS_M75_IDS.
pub const SUBPLATFORM_G11_IDS: &[u16] = &[0x56A5, 0x56A6, 0x56B0, 0x56B1, 0x56BA, 0x56BB, 0x56BC, 0x56BD, 0x5693, 0x5694, 0x5695, 0x56C1];

/// subplatform list `subplatform_g12_ids` (intel_device_info.c), from INTEL_DG2_G12_IDS.
pub const SUBPLATFORM_G12_IDS: &[u16] = &[0x56A3, 0x56A4, 0x56B2, 0x56B3, 0x5696, 0x5697];

/// subplatform list `subplatform_dg2_d_ids` (intel_device_info.c), from INTEL_DG2_D_IDS.
pub const SUBPLATFORM_DG2_D_IDS: &[u16] = &[0x56A0, 0x56A1, 0x56A2, 0x56A5, 0x56A6, 0x56B0, 0x56B1, 0x56A3, 0x56A4, 0x56B2, 0x56B3];

/// subplatform list `subplatform_arl_h_ids` (intel_device_info.c), from INTEL_ARL_H_IDS.
pub const SUBPLATFORM_ARL_H_IDS: &[u16] = &[0x7D51, 0x7DD1];

/// subplatform list `subplatform_arl_u_ids` (intel_device_info.c), from INTEL_ARL_U_IDS.
pub const SUBPLATFORM_ARL_U_IDS: &[u16] = &[0x7D41];

/// subplatform list `subplatform_arl_s_ids` (intel_device_info.c), from INTEL_ARL_S_IDS.
pub const SUBPLATFORM_ARL_S_IDS: &[u16] = &[0x7D67, 0xB640];

/// `INTEL_ICL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ICL_IDS: &[u16] = &[0x8A50, 0x8A52, 0x8A53, 0x8A54, 0x8A56, 0x8A57, 0x8A58, 0x8A59, 0x8A5A, 0x8A5B, 0x8A5C, 0x8A70, 0x8A71, 0x8A51, 0x8A5D];

/// `INTEL_EHL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_EHL_IDS: &[u16] = &[0x4541, 0x4551, 0x4555, 0x4557, 0x4570, 0x4571];

/// `INTEL_JSL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_JSL_IDS: &[u16] = &[0x4E51, 0x4E55, 0x4E57, 0x4E61, 0x4E71];

/// `INTEL_TGL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_TGL_IDS: &[u16] = &[0x9A60, 0x9A68, 0x9A70, 0x9A40, 0x9A49, 0x9A59, 0x9A78, 0x9AC0, 0x9AC9, 0x9AD9, 0x9AF8];

/// `INTEL_RKL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_RKL_IDS: &[u16] = &[0x4C80, 0x4C8A, 0x4C8B, 0x4C8C, 0x4C90, 0x4C9A];

/// `INTEL_ADLS_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ADLS_IDS: &[u16] = &[0x4680, 0x4682, 0x4688, 0x468A, 0x468B, 0x4690, 0x4692, 0x4693];

/// `INTEL_ADLP_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ADLP_IDS: &[u16] = &[0x46A0, 0x46A1, 0x46A2, 0x46A3, 0x46A6, 0x46A8, 0x46AA, 0x462A, 0x4626, 0x4628, 0x46B0, 0x46B1, 0x46B2, 0x46B3, 0x46C0, 0x46C1, 0x46C2, 0x46C3];

/// `INTEL_ADLN_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ADLN_IDS: &[u16] = &[0x46D0, 0x46D1, 0x46D2, 0x46D3, 0x46D4];

/// `INTEL_DG1_IDS()` from include/drm/intel/pciids.h.
pub const PCI_DG1_IDS: &[u16] = &[0x4905, 0x4906, 0x4907, 0x4908, 0x4909];

/// `INTEL_RPLS_IDS()` from include/drm/intel/pciids.h.
pub const PCI_RPLS_IDS: &[u16] = &[0xA780, 0xA781, 0xA782, 0xA783, 0xA788, 0xA789, 0xA78A, 0xA78B];

/// `INTEL_RPLU_IDS()` from include/drm/intel/pciids.h.
pub const PCI_RPLU_IDS: &[u16] = &[0xA721, 0xA7A1, 0xA7A9, 0xA7AC, 0xA7AD];

/// `INTEL_RPLP_IDS()` from include/drm/intel/pciids.h.
pub const PCI_RPLP_IDS: &[u16] = &[0xA720, 0xA7A0, 0xA7A8, 0xA7AA, 0xA7AB];

/// `INTEL_DG2_IDS()` from include/drm/intel/pciids.h.
pub const PCI_DG2_IDS: &[u16] = &[0x56A0, 0x56A1, 0x56A2, 0x56BE, 0x56BF, 0x5690, 0x5691, 0x5692, 0x56A5, 0x56A6, 0x56B0, 0x56B1, 0x56BA, 0x56BB, 0x56BC, 0x56BD, 0x5693, 0x5694, 0x5695, 0x56A3, 0x56A4, 0x56B2, 0x56B3, 0x5696, 0x5697];

/// `INTEL_ATS_M_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ATS_M_IDS: &[u16] = &[0x56C0, 0x56C2, 0x56C1];

/// `INTEL_ARL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_ARL_IDS: &[u16] = &[0x7D51, 0x7DD1, 0x7D41, 0x7D67, 0xB640];

/// `INTEL_MTL_IDS()` from include/drm/intel/pciids.h.
pub const PCI_MTL_IDS: &[u16] = &[0x7D40, 0x7D45, 0x7D55, 0x7D60, 0x7DD5];
