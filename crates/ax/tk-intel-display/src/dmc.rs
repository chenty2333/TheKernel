// SPDX-License-Identifier: MIT
// Translated from Linux v7.2.3 drivers/gpu/drm/i915/display/intel_dmc.c,
// dmc_firmware_default(), parse_dmc_fw(), parse_dmc_fw_header(),
// fw_info_matches_stepping(), and display-version DMC size constants.
// Copyright © 2014 Intel Corporation. Full grant: ../LICENSE-MIT.
//! Display microcontroller firmware naming, package parsing, and size limits
//! for display 12/13. Register programming is owned by the kernel adapter.

use alloc::vec::Vec;

const CSS_HEADER_BYTES: usize = 128;
const PACKAGE_HEADER_BYTES: usize = 16;
const PACKAGE_V1_ENTRIES: usize = 20;
const PACKAGE_V2_ENTRIES: usize = 32;
const DMC_V1_MMIO_COUNT: usize = 8;
const DMC_V3_MMIO_COUNT: usize = 20;
const DMC_V1_MMIO_START: u32 = 0x80000;
const DMC_MAIN_MMIO_START: u32 = 0x8f000;
const DMC_MAIN_MMIO_END: u32 = 0x8ffff;
const DMC_TGL_PIPE_A_MMIO: core::ops::RangeInclusive<u32> = 0x92000..=0x93fff;
const DMC_TGL_PIPE_B_MMIO: core::ops::RangeInclusive<u32> = 0x96000..=0x97fff;
const DMC_ADLP_PIPE_MMIO: core::ops::RangeInclusive<u32> = 0x5f000..=0x5ffff;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FirmwareError {
    Truncated,
    InvalidCssHeader,
    InvalidPackageHeader,
    InvalidDmcHeader,
    MissingMainProgram,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmcProgram {
    pub id: u8,
    pub version: u32,
    pub start_mmio: u32,
    pub mmio: Vec<(u32, u32)>,
    pub payload: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmcFirmware {
    pub version: u32,
    pub programs: Vec<DmcProgram>,
}

/// Parse the CSS package and main DMC program using the i915 v1/v2 package
/// selection and v1/v3 DMC headers. `step` and `substep` are the two ASCII
/// stepping characters (or `b'*'` when the platform does not identify them).
// upstream: intel_dmc.c parse_dmc_fw()
pub fn parse_firmware(
    data: &[u8],
    platform: DmcPlatform,
    step: u8,
    substep: u8,
) -> Result<DmcFirmware, FirmwareError> {
    if data.len() < CSS_HEADER_BYTES {
        return Err(FirmwareError::Truncated);
    }
    let css_len = word(data, 4)? as usize * 4;
    if css_len != CSS_HEADER_BYTES {
        return Err(FirmwareError::InvalidCssHeader);
    }
    let css_version = word(data, 88)?;
    let package = data.get(css_len..).ok_or(FirmwareError::Truncated)?;
    if package.len() < PACKAGE_HEADER_BYTES {
        return Err(FirmwareError::Truncated);
    }
    let package_version = package[1];
    let entry_cap = match package_version {
        1 => PACKAGE_V1_ENTRIES,
        2 => PACKAGE_V2_ENTRIES,
        _ => return Err(FirmwareError::InvalidPackageHeader),
    };
    let package_len = PACKAGE_HEADER_BYTES + entry_cap * 12;
    if package.len() < package_len {
        return Err(FirmwareError::Truncated);
    }
    if usize::from(package[0]) * 4 != package_len {
        return Err(FirmwareError::InvalidPackageHeader);
    }
    let count = (word(package, 12)? as usize).min(entry_cap);
    let mut selected = [None; 5];
    for i in 0..count {
        let entry = PACKAGE_HEADER_BYTES + i * 12;
        let dmc_id = if package_version == 1 {
            0
        } else {
            package[entry + 1]
        };
        let Some(offset_slot) = selected.get_mut(usize::from(dmc_id)) else {
            continue;
        };
        if offset_slot.is_some() {
            continue;
        }
        let fw_step = package[entry + 2];
        let fw_substep = package[entry + 3];
        if stepping_matches(fw_step, fw_substep, step, substep) {
            *offset_slot = Some(word(package, entry + 4)? as usize);
        }
    }
    if selected[0].is_none() {
        return Err(FirmwareError::MissingMainProgram);
    }
    let mut programs = Vec::new();
    for (dmc_id, offset) in selected.into_iter().enumerate() {
        let Some(offset) = offset else { continue };
        let header_start = css_len
            .checked_add(package_len)
            .and_then(|base| {
                offset
                    .checked_mul(4)
                    .and_then(|offset| base.checked_add(offset))
            })
            .ok_or(FirmwareError::Truncated)?;
        let header = data.get(header_start..).ok_or(FirmwareError::Truncated)?;
        programs.push(parse_program_header(header, platform, dmc_id as u8)?);
    }
    Ok(DmcFirmware {
        version: css_version,
        programs,
    })
}

// upstream: intel_dmc.c fw_info_matches_stepping()
fn stepping_matches(fw_step: u8, fw_substep: u8, step: u8, substep: u8) -> bool {
    (fw_substep == b'*' && step == fw_step)
        || (step == fw_step && substep == fw_substep)
        || (step == b'*' && substep == fw_substep)
        || (fw_step == b'*' && fw_substep == b'*')
}

// upstream: intel_dmc.c parse_dmc_fw_header()
fn parse_program_header(
    header: &[u8],
    platform: DmcPlatform,
    dmc_id: u8,
) -> Result<DmcProgram, FirmwareError> {
    if header.len() < 24 {
        return Err(FirmwareError::Truncated);
    }
    if word(header, 0)? != 0x4040_3e3e {
        return Err(FirmwareError::InvalidDmcHeader);
    }
    let header_version = header[5];
    let fw_size = word(header, 12)? as usize;
    let version = word(header, 16)?;
    let (header_len, start_mmio, mmio_addrs, mmio_data, max_count, v1) = match header_version {
        1 => {
            let header_len = usize::from(header[4]);
            let expected_len = 24 + 4 + DMC_V1_MMIO_COUNT * 8 + 32 + 8;
            if header_len != expected_len {
                return Err(FirmwareError::InvalidDmcHeader);
            }
            (
                header_len,
                DMC_V1_MMIO_START,
                28,
                28 + DMC_V1_MMIO_COUNT * 4,
                DMC_V1_MMIO_COUNT,
                true,
            )
        }
        3 => {
            let expected_len = 24 + 4 + 36 + 32 + 4 + DMC_V3_MMIO_COUNT * 8;
            let header_len = usize::from(header[4]) * 4;
            if header_len != expected_len {
                return Err(FirmwareError::InvalidDmcHeader);
            }
            (
                header_len,
                word(header, 24)?,
                100,
                100 + DMC_V3_MMIO_COUNT * 4,
                DMC_V3_MMIO_COUNT,
                false,
            )
        }
        _ => return Err(FirmwareError::InvalidDmcHeader),
    };
    if header.len() < header_len {
        return Err(FirmwareError::Truncated);
    }
    let mmio_count_offset = if v1 { 24 } else { 96 };
    let mmio_count = word(header, mmio_count_offset)? as usize;
    if mmio_count > max_count {
        return Err(FirmwareError::InvalidDmcHeader);
    }
    let end = header_len
        .checked_add(fw_size.checked_mul(4).ok_or(FirmwareError::Truncated)?)
        .ok_or(FirmwareError::Truncated)?;
    if header.len() < end {
        return Err(FirmwareError::Truncated);
    }
    let max_fw_size = match platform {
        DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN => 0x20000,
        _ => 0x6000,
    };
    let payload_bytes = fw_size * 4;
    if payload_bytes > max_fw_size {
        return Err(FirmwareError::InvalidDmcHeader);
    }
    let mut mmio = Vec::with_capacity(mmio_count);
    let allowed = if v1 {
        DMC_V1_MMIO_START..=0x8ffff
    } else if dmc_id == 0 {
        DMC_MAIN_MMIO_START..=DMC_MAIN_MMIO_END
    } else {
        match platform {
            DmcPlatform::AlderLakeP | DmcPlatform::AlderLakeN => DMC_ADLP_PIPE_MMIO,
            _ if dmc_id == 1 => DMC_TGL_PIPE_A_MMIO,
            _ if dmc_id == 2 => DMC_TGL_PIPE_B_MMIO,
            _ => return Err(FirmwareError::InvalidDmcHeader),
        }
    };
    for i in 0..mmio_count {
        let address = word(header, mmio_addrs + i * 4)?;
        if !allowed.contains(&address) {
            return Err(FirmwareError::InvalidDmcHeader);
        }
        mmio.push((address, word(header, mmio_data + i * 4)?));
    }
    let payload = header[header_len..end]
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();
    Ok(DmcProgram {
        id: dmc_id,
        version,
        start_mmio,
        mmio,
        payload,
    })
}

fn word(data: &[u8], offset: usize) -> Result<u32, FirmwareError> {
    let bytes = data
        .get(offset..offset.checked_add(4).ok_or(FirmwareError::Truncated)?)
        .ok_or(FirmwareError::Truncated)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

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
    use alloc::vec;

    use super::*;

    fn put_word(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn v2_firmware() -> Vec<u8> {
        let css_len = CSS_HEADER_BYTES;
        let package_len = PACKAGE_HEADER_BYTES + PACKAGE_V2_ENTRIES * 12;
        let header_len = 24 + 4 + 36 + 32 + 4 + DMC_V3_MMIO_COUNT * 8;
        let payload_len = 8;
        let program_len = header_len + payload_len;
        let mut data = vec![0; css_len + package_len + program_len + header_len + 4];
        put_word(&mut data, 4, (css_len / 4) as u32);
        put_word(&mut data, 88, 0x0002_0004);

        let package = css_len;
        data[package] = (package_len / 4) as u8;
        data[package + 1] = 2;
        put_word(&mut data, package + 12, 2);
        data[package + PACKAGE_HEADER_BYTES + 1] = 0;
        data[package + PACKAGE_HEADER_BYTES + 2] = b'*';
        data[package + PACKAGE_HEADER_BYTES + 3] = b'*';
        let pipe_entry = package + PACKAGE_HEADER_BYTES + 12;
        data[pipe_entry + 1] = 1;
        data[pipe_entry + 2] = b'*';
        data[pipe_entry + 3] = b'*';
        put_word(&mut data, pipe_entry + 4, (program_len / 4) as u32);

        let header = css_len + package_len;
        put_word(&mut data, header, 0x4040_3e3e);
        data[header + 4] = (header_len / 4) as u8;
        data[header + 5] = 3;
        put_word(&mut data, header + 12, (payload_len / 4) as u32);
        put_word(&mut data, header + 16, 0x0003_0004);
        put_word(&mut data, header + 24, 0x8f000);
        put_word(&mut data, header + 96, 1);
        put_word(&mut data, header + 100, 0x8f004);
        put_word(&mut data, header + 180, 0x1234_5678);
        put_word(&mut data, header + header_len, 0x0123_4567);
        put_word(&mut data, header + header_len + 4, 0x89ab_cdef);

        let pipe = header + program_len;
        put_word(&mut data, pipe, 0x4040_3e3e);
        data[pipe + 4] = (header_len / 4) as u8;
        data[pipe + 5] = 3;
        put_word(&mut data, pipe + 12, 1);
        put_word(&mut data, pipe + 16, 0x0003_0004);
        put_word(&mut data, pipe + 24, 0x5f000);
        put_word(&mut data, pipe + 96, 1);
        put_word(&mut data, pipe + 100, 0x5f004);
        put_word(&mut data, pipe + 180, 0x8765_4321);
        put_word(&mut data, pipe + header_len, 0xfedc_ba98);
        data
    }

    fn v1_firmware() -> Vec<u8> {
        let package_len = PACKAGE_HEADER_BYTES + PACKAGE_V1_ENTRIES * 12;
        let header_len = 24 + 4 + DMC_V1_MMIO_COUNT * 8 + 32 + 8;
        let mut data = vec![0; CSS_HEADER_BYTES + package_len + header_len + 4];
        put_word(&mut data, 4, (CSS_HEADER_BYTES / 4) as u32);
        put_word(&mut data, 88, 0x0001_0009);
        let package = CSS_HEADER_BYTES;
        data[package] = (package_len / 4) as u8;
        data[package + 1] = 1;
        put_word(&mut data, package + 12, 1);
        data[package + PACKAGE_HEADER_BYTES + 2] = b'*';
        data[package + PACKAGE_HEADER_BYTES + 3] = b'*';
        let header = CSS_HEADER_BYTES + package_len;
        put_word(&mut data, header, 0x4040_3e3e);
        data[header + 4] = header_len as u8;
        data[header + 5] = 1;
        put_word(&mut data, header + 12, 1);
        put_word(&mut data, header + 16, 0x0001_0009);
        put_word(&mut data, header + 24, 1);
        put_word(&mut data, header + 28, 0x80000);
        put_word(&mut data, header + 60, 0x55aa_55aa);
        put_word(&mut data, header + header_len, 0xaabb_ccdd);
        data
    }

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
            assert_eq!(
                files.fallback,
                Some("/lib/firmware/i915/adlp_dmc_ver2_16.bin")
            );
            assert_eq!(files.max_size, 0x20000);
        }
    }

    #[test]
    fn v2_package_selects_and_decodes_display13_main_firmware() {
        let firmware = v2_firmware();
        let firmware = parse_firmware(&firmware, DmcPlatform::AlderLakeN, b'D', b'0').unwrap();
        assert_eq!(firmware.version, 0x0002_0004);
        assert_eq!(firmware.programs.len(), 2);
        let program = &firmware.programs[0];
        assert_eq!((program.id, program.version), (0, 0x0003_0004));
        assert_eq!(program.start_mmio, 0x8f000);
        assert_eq!(program.mmio, [(0x8f004, 0x1234_5678)]);
        assert_eq!(program.payload, [0x0123_4567, 0x89ab_cdef]);
        let pipe = &firmware.programs[1];
        assert_eq!(pipe.id, 1);
        assert_eq!(pipe.start_mmio, 0x5f000);
        assert_eq!(pipe.mmio, [(0x5f004, 0x8765_4321)]);
        assert_eq!(pipe.payload, [0xfedc_ba98]);
    }

    #[test]
    fn v1_header_and_package_are_parsed_for_display12() {
        let firmware = parse_firmware(&v1_firmware(), DmcPlatform::TigerLake, b'D', b'0').unwrap();
        assert_eq!(firmware.version, 0x0001_0009);
        assert_eq!(firmware.programs.len(), 1);
        assert_eq!(firmware.programs[0].start_mmio, DMC_V1_MMIO_START);
        assert_eq!(firmware.programs[0].mmio, [(0x80000, 0x55aa_55aa)]);
        assert_eq!(firmware.programs[0].payload, [0xaabb_ccdd]);
    }

    #[test]
    fn dmc_parser_rejects_truncation_bad_mmio_and_missing_main_entry() {
        let firmware = v2_firmware();
        assert_eq!(
            parse_firmware(
                &firmware[..firmware.len() - 1],
                DmcPlatform::AlderLakeN,
                b'D',
                b'0'
            ),
            Err(FirmwareError::Truncated)
        );
        let mut bad_mmio = firmware.clone();
        let header = CSS_HEADER_BYTES + PACKAGE_HEADER_BYTES + PACKAGE_V2_ENTRIES * 12;
        put_word(&mut bad_mmio, header + 100, 0x5f000);
        assert_eq!(
            parse_firmware(&bad_mmio, DmcPlatform::AlderLakeN, b'D', b'0'),
            Err(FirmwareError::InvalidDmcHeader)
        );
        let mut no_main = firmware;
        no_main[CSS_HEADER_BYTES + PACKAGE_HEADER_BYTES + 1] = 1;
        assert_eq!(
            parse_firmware(&no_main, DmcPlatform::AlderLakeN, b'D', b'0'),
            Err(FirmwareError::MissingMainProgram)
        );
    }
}
