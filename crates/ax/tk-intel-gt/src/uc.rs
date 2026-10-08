// SPDX-License-Identifier: MIT
// Linux 7.2.3 drivers/gpu/drm/i915/gt/uc/intel_uc_fw.c:
// GuC/HuC platform firmware selection for TGL/RKL/ADL-S/ADL-P.
// Copyright © 2016-2019 Intel Corporation.
// Firmware load phases from drivers/gpu/drm/i915/gt/uc/intel_uc_fw.h.
// Copyright © 2014-2019 Intel Corporation.
// Gen12 PCI-ID mapping from include/drm/intel/pciids.h.
// Copyright 2013 Intel Corporation.
// Full MIT grant: ../LICENSE-MIT.
//! Intel Gen12 GuC/HuC firmware fetch metadata and load phase tracking.
//! Hardware transfers are performed by `guc_fw` through the GT owner.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};

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

/// i915 `intel_uc_fw_status` phase names for firmware fetch/init/upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FirmwareStatus {
    NotSupported = 0,
    Uninitialized,
    Disabled,
    Selected,
    Missing,
    Error,
    Available,
    InitFail,
    Loadable,
    LoadFail,
    Transferred,
    Running,
}

impl FirmwareStatus {
    fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::NotSupported,
            1 => Self::Uninitialized,
            2 => Self::Disabled,
            3 => Self::Selected,
            4 => Self::Missing,
            5 => Self::Error,
            6 => Self::Available,
            7 => Self::InitFail,
            8 => Self::Loadable,
            9 => Self::LoadFail,
            10 => Self::Transferred,
            11 => Self::Running,
            _ => Self::Error,
        }
    }

    fn permits(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Available, Self::Loadable | Self::InitFail)
                | (Self::Loadable, Self::LoadFail | Self::Transferred)
                | (
                    Self::Transferred,
                    Self::LoadFail | Self::Loadable | Self::Running
                )
                | (Self::Running, Self::Loadable)
                | (Self::LoadFail, Self::Loadable)
        )
    }
}

pub const ENABLE_GUC_SUBMISSION: u32 = 1 << 0;
pub const ENABLE_GUC_LOAD_HUC: u32 = 1 << 1;

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
    pub vf_version: u32,
    pub private_data_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareError {
    Missing,
    Css(CssError),
    VersionRange,
    UnexpectedVersion,
}

pub struct FirmwareImage {
    pub kind: Kind,
    pub blob: Blob,
    pub css: CssInfo,
    pub old_version: bool,
    pub bytes: Vec<u8>,
    pub(crate) status: AtomicU8,
}

impl FirmwareImage {
    pub fn status(&self) -> FirmwareStatus {
        FirmwareStatus::from_raw(self.status.load(Ordering::Acquire))
    }

    /// Move through the same fetch/init/upload status phases as i915. The
    /// upload owner decides when a reset has made a RUNNING image loadable.
    pub fn change_status(&self, next: FirmwareStatus) -> Result<(), FirmwareStatus> {
        let mut current = self.status.load(Ordering::Acquire);
        loop {
            let state = FirmwareStatus::from_raw(current);
            if !state.permits(next) {
                return Err(state);
            }
            match self.status.compare_exchange_weak(
                current,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }
}

// upstream: intel_uc_fw.c intel_uc_fw_fetch()
/// Requests candidate files in table order and validates the first file found.
pub fn load(
    platform: Platform,
    kind: Kind,
    max_bytes: usize,
    wopcm_bytes: usize,
    mut request: impl FnMut(&str, usize) -> Option<Vec<u8>>,
) -> Result<FirmwareImage, FirmwareError> {
    let mut fallback_candidate = false;
    for blob in candidates(platform, kind) {
        let Some(bytes) = request(blob.path, max_bytes) else {
            fallback_candidate = true;
            continue;
        };
        let css = parse_css(&bytes, wopcm_bytes).map_err(FirmwareError::Css)?;
        if kind == Kind::GuC {
            if !guc_versions_valid(
                css.version,
                guc_css_info(css.version, css).submission_version,
            ) {
                return Err(FirmwareError::VersionRange);
            }
        }
        let old_version = check_file_version(blob.version, css.version, false)
            .map_err(|_| FirmwareError::UnexpectedVersion)?
            || fallback_candidate;
        return Ok(FirmwareImage {
            kind,
            blob: *blob,
            css,
            old_version,
            bytes,
            status: AtomicU8::new(FirmwareStatus::Available as u8),
        });
    }
    Err(FirmwareError::Missing)
}

/// upstream: intel_uc_fw.c intel_uc_check_file_version()
/// Return whether an accepted file is older than the version table entry.
pub fn check_file_version(
    wanted: (u8, u8, u8),
    selected: (u8, u8, u8),
    overridden: bool,
) -> Result<bool, ()> {
    if wanted.0 == 0 || selected.0 == 0 {
        return Ok(false);
    }
    if selected.0 != wanted.0 {
        return if overridden { Ok(false) } else { Err(()) };
    }
    Ok((selected.1, selected.2) < (wanted.1, wanted.2))
}

// upstream: intel_uc_fw.c __check_ccs_header()
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
        vf_version: dword(68),
        private_data_bytes: dword(120) as usize,
    })
}

// upstream: intel_uc_fw.c uc_unpack_css_version()
fn unpack_css_version(value: u32) -> (u8, u8, u8) {
    (
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GucCssInfo {
    pub submission_version: (u8, u8, u8),
    pub private_data_bytes: usize,
}

// upstream: intel_uc_fw.c is_ver_8bit()
fn is_ver_8bit(version: (u8, u8, u8)) -> bool {
    version.0 < 0xff && version.1 < 0xff && version.2 < 0xff
}

// upstream: intel_uc_fw.c guc_check_version_range()
pub fn guc_versions_valid(firmware: (u8, u8, u8), submission: (u8, u8, u8)) -> bool {
    is_ver_8bit(firmware) && is_ver_8bit(submission)
}

// upstream: intel_uc_fw.c guc_read_css_info()
pub fn guc_css_info(firmware: (u8, u8, u8), css: CssInfo) -> GucCssInfo {
    let submission_version = if firmware.0 >= 70 {
        if firmware.1 >= 6 {
            // CSS vf_version packs the three ABI components into low 24 bits.
            unpack_css_version(css.vf_version)
        } else if firmware.1 >= 3 {
            (1, 1, 0)
        } else {
            (1, 0, 0)
        }
    } else if firmware.0 >= 69 {
        (0, 10, 0)
    } else {
        (0, 1, 0)
    };
    GucCssInfo {
        submission_version,
        private_data_bytes: css.private_data_bytes,
    }
}

// upstream: intel_uc_fw.c __uc_fw_auto_select() (Gen12 firmware table)
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

// upstream: intel_uc_fw.c __uc_fw_auto_select()
pub fn candidates(platform: Platform, kind: Kind) -> &'static [Blob] {
    match (platform, kind) {
        (Platform::TigerLake | Platform::RocketLake, Kind::GuC) => TGL_GUC,
        (Platform::AlderLakeS | Platform::AlderLakeN, Kind::GuC) => ADLS_GUC,
        (Platform::AlderLakeP, Kind::GuC) => ADLP_GUC,
        (Platform::TigerLake | Platform::RocketLake, Kind::HuC) => TGL_HUC,
        (Platform::AlderLakeS | Platform::AlderLakeP | Platform::AlderLakeN, Kind::HuC) => ADL_HUC,
    }
}

// upstream: intel_uc.c uc_expand_default_options()
pub fn default_enable_mask(platform: Platform) -> u32 {
    match platform {
        Platform::TigerLake | Platform::RocketLake => 0,
        Platform::AlderLakeS => ENABLE_GUC_LOAD_HUC,
        Platform::AlderLakeP | Platform::AlderLakeN => ENABLE_GUC_LOAD_HUC | ENABLE_GUC_SUBMISSION,
    }
}

/// Map Gen12 integrated-GPU IDs from `include/drm/intel/pciids.h` to the
/// platform family used by the uC firmware policy. ADL-N uses ADL-S blobs but
/// retains ADL-P/N option defaults.
pub fn platform_from_device_id(device_id: u16) -> Option<Platform> {
    const TGL: &[u16] = &[
        0x9a60, 0x9a68, 0x9a70, 0x9a40, 0x9a49, 0x9a59, 0x9a78, 0x9ac0, 0x9ac9, 0x9ad9, 0x9af8,
    ];
    const RKL: &[u16] = &[0x4c80, 0x4c8a, 0x4c8b, 0x4c8c, 0x4c90, 0x4c9a];
    const ADLS: &[u16] = &[
        0x4680, 0x4682, 0x4688, 0x468a, 0x468b, 0x4690, 0x4692, 0x4693,
    ];
    const ADLP: &[u16] = &[
        0x46a0, 0x46a1, 0x46a2, 0x46a3, 0x46a6, 0x46a8, 0x46aa, 0x462a, 0x4626, 0x4628, 0x46b0,
        0x46b1, 0x46b2, 0x46b3, 0x46c0, 0x46c1, 0x46c2, 0x46c3,
    ];
    const ADLN: &[u16] = &[0x46d0, 0x46d1, 0x46d2, 0x46d3, 0x46d4];
    if TGL.contains(&device_id) {
        Some(Platform::TigerLake)
    } else if RKL.contains(&device_id) {
        Some(Platform::RocketLake)
    } else if ADLS.contains(&device_id) {
        Some(Platform::AlderLakeS)
    } else if ADLP.contains(&device_id) {
        Some(Platform::AlderLakeP)
    } else if ADLN.contains(&device_id) {
        Some(Platform::AlderLakeN)
    } else {
        None
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

    #[test]
    fn u_c_defaults_follow_gen12_platform_policy() {
        assert_eq!(default_enable_mask(Platform::TigerLake), 0);
        assert_eq!(default_enable_mask(Platform::RocketLake), 0);
        assert_eq!(
            default_enable_mask(Platform::AlderLakeS),
            ENABLE_GUC_LOAD_HUC
        );
        assert_eq!(
            default_enable_mask(Platform::AlderLakeN),
            ENABLE_GUC_LOAD_HUC | ENABLE_GUC_SUBMISSION
        );
        assert_eq!(
            default_enable_mask(Platform::AlderLakeP),
            ENABLE_GUC_LOAD_HUC | ENABLE_GUC_SUBMISSION
        );
    }

    #[test]
    fn gen12_device_ids_select_runtime_uc_platform() {
        assert_eq!(platform_from_device_id(0x9a40), Some(Platform::TigerLake));
        assert_eq!(platform_from_device_id(0x4c80), Some(Platform::RocketLake));
        assert_eq!(platform_from_device_id(0x4680), Some(Platform::AlderLakeS));
        assert_eq!(platform_from_device_id(0x46a0), Some(Platform::AlderLakeP));
        assert_eq!(platform_from_device_id(0x46d0), Some(Platform::AlderLakeN));
        assert_eq!(platform_from_device_id(0x1234), None);
    }

    #[test]
    fn firmware_status_follows_fetch_init_upload_and_reset_phases() {
        assert!(FirmwareStatus::Available.permits(FirmwareStatus::Loadable));
        assert!(FirmwareStatus::Loadable.permits(FirmwareStatus::Transferred));
        assert!(FirmwareStatus::Transferred.permits(FirmwareStatus::Running));
        assert!(FirmwareStatus::Running.permits(FirmwareStatus::Loadable));
        assert!(!FirmwareStatus::Available.permits(FirmwareStatus::Running));
        assert!(!FirmwareStatus::Disabled.permits(FirmwareStatus::Loadable));
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
                vf_version: 0,
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

    #[test]
    fn loader_requests_files_in_upstream_preference_order_and_retains_image() {
        let mut image = css_image(64, 128);
        image[64..68].copy_from_slice(&(70u32 << 16 | 1 << 8 | 1).to_le_bytes());
        let mut requested = std::vec::Vec::new();
        let loaded = load(
            Platform::AlderLakeN,
            Kind::GuC,
            1024 * 1024,
            2 * 1024 * 1024,
            |path, max| {
                requested.push(std::string::String::from(path));
                assert_eq!(max, 1024 * 1024);
                (path == "i915/tgl_guc_70.1.1.bin").then(|| image.clone())
            },
        )
        .unwrap();
        assert_eq!(requested.len(), 2);
        assert_eq!(requested[0], "i915/tgl_guc_70.bin");
        assert_eq!(requested[1], "i915/tgl_guc_70.1.1.bin");
        assert_eq!(loaded.blob.path, "i915/tgl_guc_70.1.1.bin");
        assert!(loaded.old_version);
        assert_eq!(loaded.status(), FirmwareStatus::Available);
        assert_eq!(loaded.bytes, image);
        assert_eq!(loaded.css.version, (70, 1, 1));
    }

    #[test]
    fn guc_css_info_tracks_firmware_compatibility_rules() {
        let mut css = css_image(64, 128);
        css[68..72].copy_from_slice(&0x0002_0304u32.to_le_bytes());
        let css = parse_css(&css, 4096).unwrap();
        assert_eq!(
            guc_css_info((70, 12, 1), css),
            GucCssInfo {
                submission_version: (2, 3, 4),
                private_data_bytes: 0x2000,
            }
        );
        assert_eq!(guc_css_info((70, 3, 0), css).submission_version, (1, 1, 0));
        assert_eq!(guc_css_info((70, 1, 1), css).submission_version, (1, 0, 0));
        assert_eq!(guc_css_info((69, 0, 3), css).submission_version, (0, 10, 0));
        assert_eq!(guc_css_info((68, 1, 0), css).submission_version, (0, 1, 0));
        assert!(guc_versions_valid((70, 12, 1), (2, 3, 4)));
        assert!(!guc_versions_valid((255, 12, 1), (2, 3, 4)));
        assert!(!guc_versions_valid((70, 12, 1), (2, 255, 4)));
    }

    #[test]
    fn file_version_check_matches_major_and_marks_older_minor_patch() {
        assert_eq!(
            check_file_version((70, 12, 1), (70, 12, 1), false),
            Ok(false)
        );
        assert_eq!(check_file_version((70, 12, 1), (70, 1, 1), false), Ok(true));
        assert_eq!(check_file_version((70, 12, 1), (69, 0, 3), false), Err(()));
        assert_eq!(check_file_version((0, 0, 0), (7, 9, 3), false), Ok(false));
    }
}
