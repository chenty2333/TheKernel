// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dmc.c,
// dmc_firmware_default() and the display-version DMC size constants.
// Copyright © 2014 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Display microcontroller firmware naming and size limits for display 12/13.
//! This does not parse a blob or program DMC MMIO; those are separate port work.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DmcPlatform {
    TigerLake,
    RocketLake,
    AlderLakeS,
    AlderLakeP,
    AlderLakeN,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareFiles {
    pub preferred: &'static str,
    pub fallback: Option<&'static str>,
    pub max_size: usize,
}

/// Selects the i915 display-12/13 firmware path and input cap.
// upstream: intel_dmc.c dmc_firmware_default()
pub const fn firmware_files(platform: DmcPlatform) -> FirmwareFiles {
    match platform {
        DmcPlatform::TigerLake => FirmwareFiles {
            preferred: "/lib/firmware/i915/tgl_dmc_ver2_12.bin",
            fallback: None,
            max_size: 0x6000,
        },
        DmcPlatform::RocketLake => FirmwareFiles {
            preferred: "/lib/firmware/i915/rkl_dmc_ver2_03.bin",
            fallback: None,
            max_size: 0x6000,
        },
        DmcPlatform::AlderLakeS => FirmwareFiles {
            preferred: "/lib/firmware/i915/adls_dmc_ver2_01.bin",
            fallback: None,
            max_size: 0x6000,
        },
        DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN => FirmwareFiles {
            preferred: "/lib/firmware/i915/adlp_dmc.bin",
            fallback: Some("/lib/firmware/i915/adlp_dmc_ver2_16.bin"),
            max_size: 0x20000,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_paths_and_caps_match_i915_display_12_13_table() {
        for (platform, path) in [
            (DmcPlatform::TigerLake, "tgl_dmc_ver2_12.bin"),
            (DmcPlatform::RocketLake, "rkl_dmc_ver2_03.bin"),
            (DmcPlatform::AlderLakeS, "adls_dmc_ver2_01.bin"),
        ] {
            let files = firmware_files(platform);
            assert!(files.preferred.ends_with(path));
            assert_eq!(files.fallback, None);
            assert_eq!(files.max_size, 0x6000);
        }
        for platform in [DmcPlatform::AlderLakeP, DmcPlatform::AlderLakeN] {
            let files = firmware_files(platform);
            assert!(files.preferred.ends_with("adlp_dmc.bin"));
            assert_eq!(files.fallback, Some("/lib/firmware/i915/adlp_dmc_ver2_16.bin"));
            assert_eq!(files.max_size, 0x20000);
        }
    }
}
