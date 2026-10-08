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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CssError {
    Truncated,
    InvalidHeader,
    InvalidLength,
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CssInfo {
    pub header_bytes: usize,
    pub microcode_bytes: usize,
    pub rsa_bytes: usize,
    pub version: (u8, u8, u8),
    pub private_data_bytes: usize,
}

/// upstream: intel_uc_fw.c __check_ccs_header()
/// Validate the CSS sizes before a caller allocates DMA memory or writes MMIO.
pub fn parse_css(data: &[u8], wopcm_bytes: usize) -> Result<CssInfo, CssError> {
    const HEADER_BYTES: usize = 128;
    if data.len() < HEADER_BYTES {
        return Err(CssError::Truncated);
    }
    let dword = |offset: usize| {
        u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .expect("fixed CSS field"),
        )
    };
    let header_dwords = dword(4) as usize;
    let image_dwords = dword(24) as usize;
    let key_dwords = dword(28) as usize;
    let modulus_dwords = dword(32) as usize;
    let exponent_dwords = dword(36) as usize;
    let ancillary = key_dwords
        .checked_add(modulus_dwords)
        .and_then(|v| v.checked_add(exponent_dwords))
        .ok_or(CssError::InvalidHeader)?;
    if header_dwords.checked_sub(ancillary) != Some(HEADER_BYTES / 4) {
        return Err(CssError::InvalidHeader);
    }
    let microcode_dwords = image_dwords
        .checked_sub(header_dwords)
        .ok_or(CssError::InvalidHeader)?;
    let microcode_bytes = microcode_dwords
        .checked_mul(4)
        .ok_or(CssError::InvalidLength)?;
    let rsa_bytes = key_dwords.checked_mul(4).ok_or(CssError::InvalidLength)?;
    let expected_bytes = HEADER_BYTES
        .checked_add(microcode_bytes)
        .and_then(|v| v.checked_add(rsa_bytes))
        .ok_or(CssError::InvalidLength)?;
    if data.len() < expected_bytes {
        return Err(CssError::Truncated);
    }
    let upload_bytes = HEADER_BYTES
        .checked_add(microcode_bytes)
        .ok_or(CssError::InvalidLength)?;
    if upload_bytes >= wopcm_bytes {
        return Err(CssError::TooLarge);
    }
    let version_word = dword(64);
    Ok(CssInfo {
        header_bytes: HEADER_BYTES,
        microcode_bytes,
        rsa_bytes,
        version: (
            ((version_word >> 16) & 0xff) as u8,
            ((version_word >> 8) & 0xff) as u8,
            (version_word & 0xff) as u8,
        ),
        private_data_bytes: dword(120) as usize,
    })
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

    fn css_image(header_dwords: u32, image_dwords: u32) -> std::vec::Vec<u8> {
        let mut image = std::vec![0; image_dwords as usize * 4 + 16];
        for (offset, value) in [
            (4, header_dwords),
            (24, image_dwords),
            (28, 4),
            (32, 16),
            (36, 12),
            (64, 70 << 16 | 12 << 8 | 1),
            (120, 0x2000),
        ] {
            image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        image
    }

    #[test]
    fn css_parser_checks_component_sizes_and_wopcm_before_publish() {
        let image = css_image(64, 128);
        assert_eq!(
            parse_css(&image, 4096),
            Ok(CssInfo {
                header_bytes: 128,
                microcode_bytes: 256,
                rsa_bytes: 16,
                version: (70, 12, 1),
                private_data_bytes: 0x2000,
            })
        );
        assert_eq!(parse_css(&image[..100], 4096), Err(CssError::Truncated));
        assert_eq!(parse_css(&image, 384), Err(CssError::TooLarge));

        let mut invalid = image.clone();
        invalid[4..8].copy_from_slice(&63u32.to_le_bytes());
        assert_eq!(parse_css(&invalid, 4096), Err(CssError::InvalidHeader));

        let mut underflow = image;
        underflow[24..28].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_css(&underflow, 4096), Err(CssError::InvalidHeader));
    }
}
