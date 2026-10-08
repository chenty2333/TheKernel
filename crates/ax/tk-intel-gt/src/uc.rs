// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c:
// GuC/HuC platform firmware selection for TGL/RKL/ADL-S/ADL-P.
// Copyright © 2016-2019 Intel Corporation.
// Full MIT grant: ../LICENSE-MIT.
//! Intel Gen12 GuC/HuC firmware filename/version table.
//!
//! This is only the platform selection data from `intel_uc_fw.c`; it does not
//! parse or upload the firmware and does not claim that a uC is running.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    TigerLake,
    RocketLake,
    AlderLakeS,
    AlderLakeP,
    /// Linux 7.2.3 classifies ADL-N as Alder Lake-S for GT uC firmware.
    AlderLakeN,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    GuC,
    HuC,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Blob {
    pub path: &'static str,
    pub version: (u8, u8, u8),
    /// Upstream's legacy file naming uses all three version components.
    pub legacy: bool,
}

const TGL_GUC: &[Blob] = &[Blob {
    path: "i915/tgl_guc_70.1.1.bin",
    version: (70, 1, 1),
    legacy: true,
}];
const ADLS_GUC: &[Blob] = &[
    Blob {
        path: "i915/tgl_guc_70.bin",
        version: (70, 12, 1),
        legacy: false,
    },
    Blob {
        path: "i915/tgl_guc_70.1.1.bin",
        version: (70, 1, 1),
        legacy: true,
    },
    Blob {
        path: "i915/tgl_guc_69.0.3.bin",
        version: (69, 0, 3),
        legacy: true,
    },
];
const ADLP_GUC: &[Blob] = &[
    Blob {
        path: "i915/adlp_guc_70.bin",
        version: (70, 12, 1),
        legacy: false,
    },
    Blob {
        path: "i915/adlp_guc_70.1.1.bin",
        version: (70, 1, 1),
        legacy: true,
    },
    Blob {
        path: "i915/adlp_guc_69.0.3.bin",
        version: (69, 0, 3),
        legacy: true,
    },
];
const TGL_HUC: &[Blob] = &[Blob {
    path: "i915/tgl_huc_7.9.3.bin",
    version: (7, 9, 3),
    legacy: true,
}];
const ADL_HUC: &[Blob] = &[
    Blob {
        path: "i915/tgl_huc.bin",
        version: (0, 0, 0),
        legacy: false,
    },
    Blob {
        path: "i915/tgl_huc_7.9.3.bin",
        version: (7, 9, 3),
        legacy: true,
    },
];

/// upstream: intel_uc_fw.c __uc_fw_auto_select()
pub fn candidates(platform: Platform, kind: Kind) -> &'static [Blob] {
    match (platform, kind) {
        (Platform::TigerLake | Platform::RocketLake, Kind::GuC) => TGL_GUC,
        (Platform::AlderLakeS | Platform::AlderLakeN, Kind::GuC) => ADLS_GUC,
        (Platform::AlderLakeP, Kind::GuC) => ADLP_GUC,
        (Platform::TigerLake | Platform::RocketLake, Kind::HuC) => TGL_HUC,
        (Platform::AlderLakeS | Platform::AlderLakeP | Platform::AlderLakeN, Kind::HuC) => ADL_HUC,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adl_prefers_current_firmware_and_keeps_upstream_fallback_order() {
        assert_eq!(candidates(Platform::AlderLakeS, Kind::GuC), ADLS_GUC);
        assert_eq!(ADLS_GUC[0].path, "i915/tgl_guc_70.bin");
        assert_eq!(ADLS_GUC[1].path, "i915/tgl_guc_70.1.1.bin");
        assert_eq!(ADLS_GUC[2].path, "i915/tgl_guc_69.0.3.bin");
        assert_eq!(candidates(Platform::AlderLakeN, Kind::GuC), ADLS_GUC);
        assert_eq!(candidates(Platform::AlderLakeP, Kind::GuC), ADLP_GUC);
    }

    #[test]
    fn tgl_rkl_and_adl_huc_variants_match_upstream_table() {
        assert_eq!(candidates(Platform::TigerLake, Kind::GuC), TGL_GUC);
        assert_eq!(candidates(Platform::RocketLake, Kind::GuC), TGL_GUC);
        assert_eq!(candidates(Platform::RocketLake, Kind::HuC), TGL_HUC);
        assert_eq!(candidates(Platform::AlderLakeP, Kind::HuC), ADL_HUC);
        assert_eq!(ADL_HUC[0].path, "i915/tgl_huc.bin");
        assert_eq!(ADL_HUC[1].path, "i915/tgl_huc_7.9.3.bin");
    }
}
