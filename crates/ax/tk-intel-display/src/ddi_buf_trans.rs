// SPDX-License-Identifier: MIT
// Linux v7.2.3 drivers/gpu/drm/i915/display/intel_ddi_buf_trans.c and
// intel_ddi_buf_trans.h. Copyright © 2020 Intel Corporation.
// MIT permission text: ../LICENSE-MIT.
//! Display-12/13 DDI buffer/PHY translation data and platform selection.

use crate::{Error, device::Platform};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferPhy {
    Combo,
    Dkl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferOutput {
    DisplayPort,
    EmbeddedDisplayPort,
    Hdmi,
    Dvi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DdiBufferTransEntry {
    Combo {
        dw2_swing_sel: u8,
        dw7_n_scalar: u8,
        dw4_cursor_coeff: u8,
        dw4_post_cursor_2: u8,
        dw4_post_cursor_1: u8,
    },
    Dkl {
        vswing: u8,
        preshoot: u8,
        de_emphasis: u8,
    },
}

const fn combo(dw2: u8, dw7: u8, cursor: u8, post2: u8, post1: u8) -> DdiBufferTransEntry {
    DdiBufferTransEntry::Combo {
        dw2_swing_sel: dw2,
        dw7_n_scalar: dw7,
        dw4_cursor_coeff: cursor,
        dw4_post_cursor_2: post2,
        dw4_post_cursor_1: post1,
    }
}
const fn dkl(vswing: u8, preshoot: u8, de_emphasis: u8) -> DdiBufferTransEntry {
    DdiBufferTransEntry::Dkl {
        vswing,
        preshoot,
        de_emphasis,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdiBufferTransTable {
    pub name: &'static str,
    pub phy: BufferPhy,
    pub entries: &'static [DdiBufferTransEntry],
    pub hdmi_default_entry: Option<u8>,
    pub hobl: bool,
}

// upstream: intel_ddi_buf_trans.c _icl_combo_phy_trans_hdmi[]
const ICL_COMBO_HDMI: [DdiBufferTransEntry; 7] = [
    combo(0x0a, 0x60, 0x3f, 0, 0),
    combo(0x0b, 0x73, 0x36, 0, 9),
    combo(0x06, 0x7f, 0x31, 0, 14),
    combo(0x0b, 0x73, 0x3f, 0, 0),
    combo(0x06, 0x7f, 0x37, 0, 8),
    combo(0x06, 0x7f, 0x3f, 0, 0),
    combo(0x06, 0x7f, 0x35, 0, 10),
];

// upstream: intel_ddi_buf_trans.c _icl_combo_phy_trans_dp_hbr2_edp_hbr3[]
const ICL_DP_HBR2_EDP_HBR3: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x71, 0x2f, 0, 16),
    combo(0x06, 0x7f, 0x2b, 0, 20),
    combo(0x0a, 0x4c, 0x3f, 0, 0),
    combo(0x0c, 0x73, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x6c, 0x3c, 0, 3),
    combo(0x06, 0x7f, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _icl_combo_phy_trans_edp_hbr2[]
const ICL_EDP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(0, 0x7f, 0x3f, 0, 0),
    combo(8, 0x7f, 0x38, 0, 7),
    combo(1, 0x7f, 0x33, 0, 12),
    combo(9, 0x7f, 0x31, 0, 14),
    combo(8, 0x7f, 0x3f, 0, 0),
    combo(1, 0x7f, 0x38, 0, 7),
    combo(9, 0x7f, 0x35, 0, 10),
    combo(1, 0x7f, 0x3f, 0, 0),
    combo(9, 0x7f, 0x38, 0, 7),
    combo(9, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _tgl_combo_phy_trans_dp_hbr[]
const TGL_DP_HBR: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x32, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x71, 0x2f, 0, 16),
    combo(0x06, 0x7d, 0x2b, 0, 20),
    combo(0x0a, 0x4c, 0x3f, 0, 0),
    combo(0x0c, 0x73, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x6c, 0x3c, 0, 3),
    combo(0x06, 0x7f, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _tgl_combo_phy_trans_dp_hbr2[]
const TGL_DP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x63, 0x2f, 0, 16),
    combo(0x06, 0x7f, 0x2b, 0, 20),
    combo(0x0a, 0x47, 0x3f, 0, 0),
    combo(0x0c, 0x63, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x61, 0x3c, 0, 3),
    combo(0x06, 0x7b, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _tgl_combo_phy_trans_edp_hbr2_hobl[]
const TGL_EDP_HOBL: [DdiBufferTransEntry; 9] = [combo(6, 0x7f, 0x3f, 0, 0); 9];

// upstream: intel_ddi_buf_trans.c _rkl_combo_phy_trans_dp_hbr[]
const RKL_DP_HBR: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x2f, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x63, 0x2f, 0, 16),
    combo(0x06, 0x7d, 0x2a, 0, 21),
    combo(0x0a, 0x4c, 0x3f, 0, 0),
    combo(0x0c, 0x73, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x6e, 0x3e, 0, 1),
    combo(0x06, 0x7f, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _rkl_combo_phy_trans_dp_hbr2_hbr3[]
const RKL_DP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x50, 0x38, 0, 7),
    combo(0x0c, 0x61, 0x33, 0, 12),
    combo(0x06, 0x7f, 0x2e, 0, 17),
    combo(0x0a, 0x47, 0x3f, 0, 0),
    combo(0x0c, 0x5f, 0x38, 0, 7),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x5f, 0x3f, 0, 0),
    combo(0x06, 0x7e, 0x36, 0, 9),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _adls_combo_phy_trans_dp_hbr2_hbr3[]
const ADLS_DP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x63, 0x31, 0, 14),
    combo(0x06, 0x7f, 0x2c, 0, 19),
    combo(0x0a, 0x47, 0x3f, 0, 0),
    combo(0x0c, 0x63, 0x37, 0, 8),
    combo(0x06, 0x73, 0x32, 0, 13),
    combo(0x0c, 0x58, 0x3f, 0, 0),
    combo(0x06, 0x7f, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _adls_combo_phy_trans_edp_hbr2[]
const ADLS_EDP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(9, 0x73, 0x3d, 0, 2),
    combo(9, 0x7a, 0x3c, 0, 3),
    combo(9, 0x7f, 0x3b, 0, 4),
    combo(4, 0x6c, 0x33, 0, 12),
    combo(2, 0x73, 0x3a, 0, 5),
    combo(2, 0x7c, 0x38, 0, 7),
    combo(4, 0x5a, 0x36, 0, 9),
    combo(4, 0x57, 0x3d, 0, 2),
    combo(4, 0x65, 0x38, 0, 7),
    combo(4, 0x6c, 0x3a, 0, 5),
];

// upstream: intel_ddi_buf_trans.c _adls_combo_phy_trans_edp_hbr3[]
const ADLS_EDP_HBR3: [DdiBufferTransEntry; 10] = ADLS_DP_HBR2;

// upstream: intel_ddi_buf_trans.c _adlp_combo_phy_trans_dp_hbr[]
const ADLP_DP_HBR: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x71, 0x31, 0, 14),
    combo(0x06, 0x7f, 0x2c, 0, 19),
    combo(0x0a, 0x4c, 0x3f, 0, 0),
    combo(0x0c, 0x73, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x2f, 0, 16),
    combo(0x0c, 0x7c, 0x3c, 0, 3),
    combo(0x06, 0x7f, 0x35, 0, 10),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _adlp_combo_phy_trans_dp_hbr2_hbr3[]
const ADLP_DP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(0x0a, 0x35, 0x3f, 0, 0),
    combo(0x0a, 0x4f, 0x37, 0, 8),
    combo(0x0c, 0x71, 0x30, 0, 15),
    combo(0x06, 0x7f, 0x2b, 0, 20),
    combo(0x0a, 0x4c, 0x3f, 0, 0),
    combo(0x0c, 0x73, 0x34, 0, 11),
    combo(0x06, 0x7f, 0x30, 0, 15),
    combo(0x0c, 0x63, 0x3f, 0, 0),
    combo(0x06, 0x7f, 0x38, 0, 7),
    combo(0x06, 0x7f, 0x3f, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _adlp_combo_phy_trans_edp_hbr2[]
const ADLP_EDP_HBR2: [DdiBufferTransEntry; 10] = [
    combo(4, 0x50, 0x38, 0, 7),
    combo(4, 0x58, 0x35, 0, 10),
    combo(4, 0x60, 0x34, 0, 11),
    combo(4, 0x6a, 0x32, 0, 13),
    combo(4, 0x5e, 0x38, 0, 7),
    combo(4, 0x61, 0x36, 0, 9),
    combo(4, 0x6b, 0x34, 0, 11),
    combo(4, 0x69, 0x39, 0, 6),
    combo(4, 0x73, 0x37, 0, 8),
    combo(4, 0x7a, 0x38, 0, 7),
];

// upstream: intel_ddi_buf_trans.c _tgl_dkl_phy_trans_dp_hbr[]
const TGL_DKL_DP_HBR: [DdiBufferTransEntry; 10] = [
    dkl(7, 0, 0),
    dkl(5, 0, 5),
    dkl(2, 0, 11),
    dkl(0, 0, 24),
    dkl(5, 0, 0),
    dkl(2, 0, 8),
    dkl(0, 0, 20),
    dkl(2, 0, 0),
    dkl(0, 0, 11),
    dkl(0, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _tgl_dkl_phy_trans_dp_hbr2[]
const TGL_DKL_DP_HBR2: [DdiBufferTransEntry; 10] = [
    dkl(7, 0, 0),
    dkl(5, 0, 5),
    dkl(2, 0, 11),
    dkl(0, 0, 25),
    dkl(5, 0, 0),
    dkl(2, 0, 8),
    dkl(0, 0, 20),
    dkl(2, 0, 0),
    dkl(0, 0, 11),
    dkl(0, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _tgl_dkl_phy_trans_hdmi[]
const TGL_DKL_HDMI: [DdiBufferTransEntry; 10] = [
    dkl(7, 0, 0),
    dkl(6, 0, 0),
    dkl(4, 0, 0),
    dkl(2, 0, 0),
    dkl(0, 0, 0),
    dkl(0, 0, 5),
    dkl(0, 0, 6),
    dkl(0, 0, 7),
    dkl(0, 0, 8),
    dkl(0, 0, 10),
];

// upstream: intel_ddi_buf_trans.c _adlp_dkl_phy_trans_dp_hbr[]
const ADLP_DKL_DP_HBR: [DdiBufferTransEntry; 10] = [
    dkl(7, 0, 1),
    dkl(5, 0, 6),
    dkl(2, 0, 11),
    dkl(0, 0, 23),
    dkl(5, 0, 0),
    dkl(2, 0, 8),
    dkl(0, 0, 20),
    dkl(2, 0, 0),
    dkl(0, 0, 11),
    dkl(0, 0, 0),
];

// upstream: intel_ddi_buf_trans.c _adlp_dkl_phy_trans_dp_hbr2_hbr3[]
const ADLP_DKL_DP_HBR2: [DdiBufferTransEntry; 10] = [
    dkl(7, 0, 0),
    dkl(5, 0, 4),
    dkl(2, 0, 10),
    dkl(0, 0, 24),
    dkl(5, 0, 0),
    dkl(2, 0, 6),
    dkl(0, 0, 20),
    dkl(2, 0, 0),
    dkl(0, 0, 9),
    dkl(0, 0, 0),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DdiBufferTransRequest {
    pub platform: Platform,
    pub phy: BufferPhy,
    pub output: BufferOutput,
    pub port_clock_khz: u32,
    pub use_edp_low_vswing: bool,
    pub use_edp_hobl: bool,
}

const fn table(
    name: &'static str,
    phy: BufferPhy,
    entries: &'static [DdiBufferTransEntry],
    hdmi_default_entry: Option<u8>,
    hobl: bool,
) -> DdiBufferTransTable {
    DdiBufferTransTable {
        name,
        phy,
        entries,
        hdmi_default_entry,
        hobl,
    }
}

const TGL_HOBL_TABLE: DdiBufferTransTable = table(
    "tgl_combo_phy_trans_edp_hbr2_hobl",
    BufferPhy::Combo,
    &TGL_EDP_HOBL,
    None,
    true,
);

/// Choose the same transition table family as i915 for display-12/13 ports.
/// The two EDP override booleans come from VBT/panel policy, as in the caller.
// upstream: intel_ddi_buf_trans.c intel_ddi_buf_trans_get()/intel_ddi_buf_trans_init()
// upstream: intel_ddi_buf_trans.c intel_ddi_buf_trans_get()
pub fn intel_ddi_buf_trans_get(
    request: DdiBufferTransRequest,
) -> Result<DdiBufferTransTable, Error> {
    let is_hdmi = matches!(request.output, BufferOutput::Hdmi | BufferOutput::Dvi);
    match (request.platform, request.phy, request.output) {
        (Platform::AlderLakeP | Platform::AlderLakeN, BufferPhy::Combo, _) if is_hdmi => Ok(table(
            "icl_combo_phy_trans_hdmi",
            BufferPhy::Combo,
            &ICL_COMBO_HDMI,
            Some(6),
            false,
        )),
        (
            Platform::AlderLakeP | Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort,
        ) if request.port_clock_khz > 540_000 => Ok(table(
            "adlp_combo_phy_trans_edp_hbr3",
            BufferPhy::Combo,
            &ADLP_DP_HBR2,
            None,
            false,
        )),
        (
            Platform::AlderLakeP | Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort,
        ) if request.use_edp_hobl => Ok(TGL_HOBL_TABLE),
        (
            Platform::AlderLakeP | Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort,
        ) if request.use_edp_low_vswing => Ok(table(
            "adlp_combo_phy_trans_edp_up_to_hbr2",
            BufferPhy::Combo,
            &ADLP_EDP_HBR2,
            None,
            false,
        )),
        (
            Platform::AlderLakeP | Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort | BufferOutput::DisplayPort,
        ) if request.port_clock_khz > 270_000 => Ok(table(
            "adlp_combo_phy_trans_dp_hbr2_hbr3",
            BufferPhy::Combo,
            &ADLP_DP_HBR2,
            None,
            false,
        )),
        (
            Platform::AlderLakeP | Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort | BufferOutput::DisplayPort,
        ) => Ok(table(
            "adlp_combo_phy_trans_dp_hbr",
            BufferPhy::Combo,
            &ADLP_DP_HBR,
            None,
            false,
        )),
        (Platform::AlderLakeP | Platform::AlderLakeN, BufferPhy::Dkl, _) if is_hdmi => Ok(table(
            "tgl_dkl_phy_trans_hdmi",
            BufferPhy::Dkl,
            &TGL_DKL_HDMI,
            Some(9),
            false,
        )),
        (Platform::AlderLakeP | Platform::AlderLakeN, BufferPhy::Dkl, _)
            if request.port_clock_khz > 270_000 =>
        {
            Ok(table(
                "adlp_dkl_phy_trans_dp_hbr2_hbr3",
                BufferPhy::Dkl,
                &ADLP_DKL_DP_HBR2,
                None,
                false,
            ))
        }
        (Platform::AlderLakeP | Platform::AlderLakeN, BufferPhy::Dkl, _) => Ok(table(
            "adlp_dkl_phy_trans_dp_hbr",
            BufferPhy::Dkl,
            &ADLP_DKL_DP_HBR,
            None,
            false,
        )),
        (Platform::AlderLakeS, BufferPhy::Combo, _) if is_hdmi => Ok(table(
            "icl_combo_phy_trans_hdmi",
            BufferPhy::Combo,
            &ICL_COMBO_HDMI,
            Some(6),
            false,
        )),
        (Platform::AlderLakeS, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.port_clock_khz > 540_000 =>
        {
            Ok(table(
                "adls_combo_phy_trans_edp_hbr3",
                BufferPhy::Combo,
                &ADLS_EDP_HBR3,
                None,
                false,
            ))
        }
        (Platform::AlderLakeS, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_hobl =>
        {
            Ok(TGL_HOBL_TABLE)
        }
        (Platform::AlderLakeS, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_low_vswing =>
        {
            Ok(table(
                "adls_combo_phy_trans_edp_hbr2",
                BufferPhy::Combo,
                &ADLS_EDP_HBR2,
                None,
                false,
            ))
        }
        (
            Platform::AlderLakeS,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) if request.port_clock_khz > 270_000 => Ok(table(
            "adls_combo_phy_trans_dp_hbr2_hbr3",
            BufferPhy::Combo,
            &ADLS_DP_HBR2,
            None,
            false,
        )),
        (
            Platform::AlderLakeS,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) => Ok(table(
            "tgl_combo_phy_trans_dp_hbr",
            BufferPhy::Combo,
            &TGL_DP_HBR,
            None,
            false,
        )),
        (Platform::RocketLake, BufferPhy::Combo, _) if is_hdmi => Ok(table(
            "icl_combo_phy_trans_hdmi",
            BufferPhy::Combo,
            &ICL_COMBO_HDMI,
            Some(6),
            false,
        )),
        (Platform::RocketLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.port_clock_khz > 540_000 =>
        {
            Ok(table(
                "icl_combo_phy_trans_dp_hbr2_edp_hbr3",
                BufferPhy::Combo,
                &ICL_DP_HBR2_EDP_HBR3,
                None,
                false,
            ))
        }
        (Platform::RocketLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_hobl =>
        {
            Ok(TGL_HOBL_TABLE)
        }
        (Platform::RocketLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_low_vswing =>
        {
            Ok(table(
                "icl_combo_phy_trans_edp_hbr2",
                BufferPhy::Combo,
                &ICL_EDP_HBR2,
                None,
                false,
            ))
        }
        (
            Platform::RocketLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) if request.port_clock_khz > 270_000 => Ok(table(
            "rkl_combo_phy_trans_dp_hbr2_hbr3",
            BufferPhy::Combo,
            &RKL_DP_HBR2,
            None,
            false,
        )),
        (
            Platform::RocketLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) => Ok(table(
            "rkl_combo_phy_trans_dp_hbr",
            BufferPhy::Combo,
            &RKL_DP_HBR,
            None,
            false,
        )),
        (Platform::TigerLake, BufferPhy::Combo, _) if is_hdmi => Ok(table(
            "icl_combo_phy_trans_hdmi",
            BufferPhy::Combo,
            &ICL_COMBO_HDMI,
            Some(6),
            false,
        )),
        (Platform::TigerLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.port_clock_khz > 540_000 =>
        {
            Ok(table(
                "icl_combo_phy_trans_dp_hbr2_edp_hbr3",
                BufferPhy::Combo,
                &ICL_DP_HBR2_EDP_HBR3,
                None,
                false,
            ))
        }
        (Platform::TigerLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_hobl =>
        {
            Ok(TGL_HOBL_TABLE)
        }
        (Platform::TigerLake, BufferPhy::Combo, BufferOutput::EmbeddedDisplayPort)
            if request.use_edp_low_vswing =>
        {
            Ok(table(
                "icl_combo_phy_trans_edp_hbr2",
                BufferPhy::Combo,
                &ICL_EDP_HBR2,
                None,
                false,
            ))
        }
        (
            Platform::TigerLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) if request.port_clock_khz > 270_000 => Ok(table(
            "tgl_combo_phy_trans_dp_hbr2",
            BufferPhy::Combo,
            &TGL_DP_HBR2,
            None,
            false,
        )),
        (
            Platform::TigerLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort | BufferOutput::EmbeddedDisplayPort,
        ) => Ok(table(
            "tgl_combo_phy_trans_dp_hbr",
            BufferPhy::Combo,
            &TGL_DP_HBR,
            None,
            false,
        )),
        (Platform::TigerLake, BufferPhy::Dkl, _) if is_hdmi => Ok(table(
            "tgl_dkl_phy_trans_hdmi",
            BufferPhy::Dkl,
            &TGL_DKL_HDMI,
            Some(9),
            false,
        )),
        (Platform::TigerLake, BufferPhy::Dkl, _) if request.port_clock_khz > 270_000 => Ok(table(
            "tgl_dkl_phy_trans_dp_hbr2",
            BufferPhy::Dkl,
            &TGL_DKL_DP_HBR2,
            None,
            false,
        )),
        (Platform::TigerLake, BufferPhy::Dkl, _) => Ok(table(
            "tgl_dkl_phy_trans_dp_hbr",
            BufferPhy::Dkl,
            &TGL_DKL_DP_HBR,
            None,
            false,
        )),
        _ => Err(Error::Refused),
    }
}

/// Source-shaped convenience for callers that only need the table's entries.
// upstream: intel_ddi_buf_trans.c intel_get_buf_trans()
// upstream: intel_ddi_buf_trans.c intel_get_buf_trans()
pub fn intel_get_buf_trans(table: DdiBufferTransTable) -> &'static [DdiBufferTransEntry] {
    table.entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        platform: Platform,
        phy: BufferPhy,
        output: BufferOutput,
        clock: u32,
    ) -> DdiBufferTransRequest {
        DdiBufferTransRequest {
            platform,
            phy,
            output,
            port_clock_khz: clock,
            use_edp_low_vswing: false,
            use_edp_hobl: false,
        }
    }

    #[test]
    fn alderlake_n_selects_source_hdmi_dp_edp_and_dkl_tables() {
        let hdmi = intel_ddi_buf_trans_get(request(
            Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::Hdmi,
            148_500,
        ))
        .unwrap();
        assert_eq!(hdmi.name, "icl_combo_phy_trans_hdmi");
        assert_eq!(hdmi.hdmi_default_entry, Some(6));
        assert_eq!(hdmi.entries.len(), 7);
        assert_eq!(hdmi.entries[6], combo(0x06, 0x7f, 0x35, 0, 10));

        let dp_rbr = intel_ddi_buf_trans_get(request(
            Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::DisplayPort,
            162_000,
        ))
        .unwrap();
        let dp_hbr2 = intel_ddi_buf_trans_get(request(
            Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::DisplayPort,
            540_000,
        ))
        .unwrap();
        assert_eq!(dp_rbr.name, "adlp_combo_phy_trans_dp_hbr");
        assert_eq!(dp_hbr2.name, "adlp_combo_phy_trans_dp_hbr2_hbr3");
        assert_eq!(dp_hbr2.entries.len(), 10);

        let mut edp_request = request(
            Platform::AlderLakeN,
            BufferPhy::Combo,
            BufferOutput::EmbeddedDisplayPort,
            270_000,
        );
        edp_request.use_edp_low_vswing = true;
        assert_eq!(
            intel_ddi_buf_trans_get(edp_request).unwrap().name,
            "adlp_combo_phy_trans_edp_up_to_hbr2"
        );
        edp_request.use_edp_hobl = true;
        assert!(intel_ddi_buf_trans_get(edp_request).unwrap().hobl);

        let dkl = intel_ddi_buf_trans_get(request(
            Platform::AlderLakeN,
            BufferPhy::Dkl,
            BufferOutput::DisplayPort,
            540_000,
        ))
        .unwrap();
        assert_eq!(dkl.name, "adlp_dkl_phy_trans_dp_hbr2_hbr3");
        assert!(matches!(
            dkl.entries[0],
            DdiBufferTransEntry::Dkl { vswing: 7, .. }
        ));
    }

    #[test]
    fn other_display12_platform_tables_keep_their_own_hbr_data() {
        let tgl = intel_ddi_buf_trans_get(request(
            Platform::TigerLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort,
            270_000,
        ))
        .unwrap();
        let rkl = intel_ddi_buf_trans_get(request(
            Platform::RocketLake,
            BufferPhy::Combo,
            BufferOutput::DisplayPort,
            162_000,
        ))
        .unwrap();
        let adls = intel_ddi_buf_trans_get(request(
            Platform::AlderLakeS,
            BufferPhy::Combo,
            BufferOutput::DisplayPort,
            162_000,
        ))
        .unwrap();
        assert_eq!(tgl.name, "tgl_combo_phy_trans_dp_hbr");
        assert_eq!(rkl.name, "rkl_combo_phy_trans_dp_hbr");
        assert_eq!(adls.name, "tgl_combo_phy_trans_dp_hbr");
        assert_ne!(intel_get_buf_trans(tgl)[0], intel_get_buf_trans(rkl)[0]);
        assert!(
            intel_ddi_buf_trans_get(request(
                Platform::TigerLake,
                BufferPhy::Dkl,
                BufferOutput::Hdmi,
                148_500,
            ))
            .unwrap()
            .hdmi_default_entry
            .is_some()
        );
    }
}
