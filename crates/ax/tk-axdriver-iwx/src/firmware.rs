//! TLV firmware image parsing translated from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230, functions
//! `iwx_read_firmware()` and `iwx_firmware_store_section()`; header and TLV
//! layouts from `sys/dev/pci/if_iwxreg.h`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use alloc::{format, vec::Vec};

const TLV_MAGIC: u32 = 0x0a4c_5749;
const AX210_UCODE_API: u32 = 89;
const HEADER_LEN: usize = 88;
const SECTION_LIMIT: usize = 69;
const PROBE_MAX: u32 = 512;
const DEFAULT_SCAN_CHANNELS: u32 = 40;
const MAX_SCAN_CHANNELS: u32 = 67;
const TLV_PROBE_MAX_LEN: u32 = 6;
const TLV_PAN: u32 = 7;
const TLV_FLAGS: u32 = 18;
const TLV_SEC_RT: u32 = 19;
const TLV_SEC_INIT: u32 = 20;
const TLV_SEC_WOWLAN: u32 = 21;
const TLV_PHY_SKU: u32 = 23;
const TLV_API_CHANGES_SET: u32 = 29;
const TLV_ENABLED_CAPABILITIES: u32 = 30;
const TLV_N_SCAN_CHANNELS: u32 = 31;
const TLV_SEC_RT_USNIFFER: u32 = 34;
const TLV_FW_VERSION: u32 = 36;
const TLV_PNVM_DATA: u32 = 74;
const TLV_DEF_CALIB: u32 = 22;
const TLV_HW_TYPE: u32 = 58;
const TLV_PNVM_VERSION: u32 = 62;
const TLV_PNVM_SKU: u32 = 64;
const TLV_CSCHEME: u32 = 28;
const TLV_NUM_OF_CPU: u32 = 27;
const TLV_PAGING: u32 = 32;
const TLV_CMD_VERSIONS: u32 = 48;
const TLV_FW_DBG_DEST: u32 = 38;
const TLV_FW_DBG_CONF: u32 = 39;
const TLV_UMAC_DEBUG_ADDRS: u32 = 54;
const TLV_LMAC_DEBUG_ADDRS: u32 = 55;
const MAX_CMD_VERSIONS: usize = 704;
const UCODE_TYPE_MAX: usize = 4;
const FW_DBG_CONF_MAX: usize = 32;
const DBG_DEST_V1_HEADER_LEN: usize = 18;
const FW_CIPHER_SCHEME_SIZE: usize = 13;
const FW_ADDR_CACHE_CONTROL: u32 = 0xc000_0000;
const TLV_IML: u32 = 52;
const TLV_PAN_FLAG: u32 = 1;
const FW_CMD_VER_UNKNOWN: u8 = 99;
const CPU1_CPU2_SEPARATOR: u32 = 0xffff_cccc;
const PAGING_SEPARATOR: u32 = 0xaaaa_bbbb;

/// Firmware microcode section type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionType {
    Regular,
    Init,
    Wowlan,
    RegularUsniffer,
}

impl SectionType {
    const fn index(self) -> usize {
        match self {
            Self::Regular => 0,
            Self::Init => 1,
            Self::Wowlan => 2,
            Self::RegularUsniffer => 3,
        }
    }
}

/// One loadable microcode section, including the device load address prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareSection {
    pub kind: SectionType,
    pub device_offset: u32,
    pub bytes: Vec<u8>,
}

/// `IWX_UCODE_TLV_DEF_CALIB` default calibration triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultCalibration {
    pub flow_trigger: u32,
    pub event_trigger: u32,
}

/// Parsed firmware facts used to configure and stage the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareImage {
    pub version: [u32; 3],
    pub sections: Vec<FirmwareSection>,
    pub section_counts: [usize; UCODE_TYPE_MAX],
    pub default_calibration: [Option<DefaultCalibration>; UCODE_TYPE_MAX],
    pub capability_flags: u32,
    pub probe_max_len: u32,
    pub scan_channels: u32,
    pub phy_config: Option<u32>,
    pub api: [u32; 4],
    pub enabled_capabilities: [u32; 5],
    pub pnvm: Option<Vec<u8>>,
    pub iml: Option<Vec<u8>>,
    pub command_versions: Vec<[u8; 4]>,
    pub umac_error_event_table: Option<u32>,
    pub lmac_error_event_table: Option<u32>,
    /// First supported `FW_DBG_DEST` TLV, retained for later debug handling.
    pub debug_dest: Option<Vec<u8>>,
    /// Debug configuration TLVs indexed by their firmware-provided ID.
    pub debug_configs: [Option<Vec<u8>>; FW_DBG_CONF_MAX],
}

/// The selected PNVM SKU and firmware segments for a hardware RF identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnvmImage {
    pub version: u32,
    pub total_size: usize,
    pub segments: Vec<Vec<u8>>,
}

/// Firmware parser failure corresponding to the driver's invalid-image path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareError {
    InvalidImage,
    TooManySections,
    InvalidProbeLength,
    InvalidVersion,
    InvalidCapabilities,
    InvalidScanChannels,
    UnsupportedTlv(u32),
    UnsupportedApi(u32),
}

impl FirmwareImage {
    /// Parse the TLV image header and firmware TLVs.
    // upstream: if_iwx.c iwx_read_firmware()
    pub fn parse(bytes: &[u8]) -> Result<Self, FirmwareError> {
        if bytes.len() < HEADER_LEN || read_u32(bytes, 0)? != 0 || read_u32(bytes, 4)? != TLV_MAGIC
        {
            return Err(FirmwareError::InvalidImage);
        }
        let packed_version = read_u32(bytes, 72)?;
        let api = (packed_version >> 8) & 0xff;
        // Intentional scope restriction: this product enables the Linux 7.2.3
        // AX210 firmware API 89 only (that source sets min=max=89).
        if api != AX210_UCODE_API {
            return Err(FirmwareError::UnsupportedApi(api));
        }
        let mut image = Self {
            version: [
                (packed_version >> 24) & 0xff,
                (packed_version >> 16) & 0xff,
                api,
            ],
            sections: Vec::new(),
            section_counts: [0; UCODE_TYPE_MAX],
            default_calibration: [None; UCODE_TYPE_MAX],
            capability_flags: 0,
            probe_max_len: 0,
            scan_channels: DEFAULT_SCAN_CHANNELS,
            phy_config: None,
            api: [0; 4],
            enabled_capabilities: [0; 5],
            pnvm: None,
            iml: None,
            command_versions: Vec::new(),
            umac_error_event_table: None,
            lmac_error_event_table: None,
            debug_dest: None,
            debug_configs: core::array::from_fn(|_| None),
        };
        let mut cursor = HEADER_LEN;
        while bytes.len().saturating_sub(cursor) >= 8 {
            let kind = read_u32(bytes, cursor)?;
            let len = read_u32(bytes, cursor + 4)? as usize;
            cursor += 8;
            let data_end = cursor.checked_add(len).ok_or(FirmwareError::InvalidImage)?;
            if data_end > bytes.len() {
                return Err(FirmwareError::InvalidImage);
            }
            let data = &bytes[cursor..data_end];
            match kind {
                TLV_PROBE_MAX_LEN => {
                    let value = read_tlv_u32(data)?;
                    if value > PROBE_MAX {
                        return Err(FirmwareError::InvalidProbeLength);
                    }
                    image.probe_max_len = value;
                }
                TLV_PAN => {
                    if !data.is_empty() {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.capability_flags |= TLV_PAN_FLAG;
                }
                TLV_FLAGS => image.capability_flags = read_tlv_u32(data)?,
                TLV_SEC_RT => store_section(&mut image, SectionType::Regular, data)?,
                TLV_SEC_INIT => store_section(&mut image, SectionType::Init, data)?,
                TLV_SEC_WOWLAN => store_section(&mut image, SectionType::Wowlan, data)?,
                TLV_SEC_RT_USNIFFER => {
                    store_section(&mut image, SectionType::RegularUsniffer, data)?
                }
                TLV_CSCHEME => validate_cipher_schemes(data)?,
                TLV_NUM_OF_CPU => {
                    let cpus = read_tlv_u32(data)?;
                    if data.len() != 4 || !(1..=2).contains(&cpus) {
                        return Err(FirmwareError::InvalidImage);
                    }
                }
                TLV_DEF_CALIB => {
                    if data.len() != 12 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    let kind = read_u32(data, 0)? as usize;
                    if kind >= UCODE_TYPE_MAX {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.default_calibration[kind] = Some(DefaultCalibration {
                        flow_trigger: read_u32(data, 4)?,
                        event_trigger: read_u32(data, 8)?,
                    });
                }
                TLV_PHY_SKU => {
                    if data.len() != 4 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.phy_config = Some(read_tlv_u32(data)?);
                }
                TLV_API_CHANGES_SET => set_bitmap(&mut image.api, data)?,
                TLV_ENABLED_CAPABILITIES => set_bitmap(&mut image.enabled_capabilities, data)?,
                TLV_PAGING => {
                    if data.len() != 4 {
                        return Err(FirmwareError::InvalidImage);
                    }
                }
                TLV_CMD_VERSIONS => {
                    // The source rounds a trailing partial four-byte command
                    // entry down, rejects duplicates, and bounds its array.
                    if !image.command_versions.is_empty() {
                        return Err(FirmwareError::InvalidImage);
                    }
                    let count = (data.len() / 4).min(MAX_CMD_VERSIONS);
                    if data.len() / 4 > MAX_CMD_VERSIONS {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.command_versions.reserve(count);
                    for entry in data[..count * 4].as_chunks::<4>().0 {
                        image.command_versions.push(*entry);
                    }
                }
                TLV_FW_DBG_DEST => {
                    if data.len() < DBG_DEST_V1_HEADER_LEN {
                        return Err(FirmwareError::InvalidImage);
                    }
                    if data.first().copied().unwrap_or(1) != 0 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    if image.debug_dest.is_none() {
                        image.debug_dest = Some(data.to_vec());
                    }
                }
                TLV_FW_DBG_CONF => {
                    if image.debug_dest.is_some() {
                        let id = *data.first().ok_or(FirmwareError::InvalidImage)? as usize;
                        if id < FW_DBG_CONF_MAX && image.debug_configs[id].is_none() {
                            image.debug_configs[id] = Some(data.to_vec());
                        }
                    }
                }
                TLV_UMAC_DEBUG_ADDRS => {
                    if data.len() != 8 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.umac_error_event_table =
                        Some(read_u32(data, 0)? & !FW_ADDR_CACHE_CONTROL);
                }
                TLV_LMAC_DEBUG_ADDRS => {
                    if data.len() != 32 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.lmac_error_event_table =
                        Some(read_u32(data, 0)? & !FW_ADDR_CACHE_CONTROL);
                }
                TLV_N_SCAN_CHANNELS => {
                    if data.len() != 4 {
                        return Err(FirmwareError::InvalidImage);
                    }
                    image.scan_channels = read_tlv_u32(data)?;
                    if image.scan_channels > MAX_SCAN_CHANNELS {
                        return Err(FirmwareError::InvalidScanChannels);
                    }
                }
                TLV_FW_VERSION => {
                    if data.len() != 12 {
                        return Err(FirmwareError::InvalidVersion);
                    }
                    image.version = [read_u32(data, 0)?, read_u32(data, 4)?, read_u32(data, 8)?];
                }
                TLV_PNVM_DATA => {
                    if image.pnvm.is_none() {
                        image.pnvm = Some(data.to_vec());
                    }
                }
                TLV_IML => image.iml = Some(data.to_vec()),
                // Accepted but unused by the current iwx path, as in OpenBSD.
                35
                | 50
                | 51
                | 57
                | 58
                | 60
                | 61
                | 65
                | 69
                | 66
                | 67
                | 68
                | 0x100
                | 0x102
                | 0x1000000
                | 0x1000002..=0x100000a
                | 0x100000b
                | 0x100000c
                | 0x101
                | 1092 => {}
                _ => return Err(FirmwareError::UnsupportedTlv(kind)),
            }
            let padded = len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
            let padding = padded - len;
            if padding > bytes.len().saturating_sub(data_end) {
                // upstream: if_iwx.c iwx_read_firmware() stops on a missing
                // final alignment pad, retaining the TLV already parsed.
                break;
            }
            cursor = data_end + padding;
        }
        Ok(image)
    }

    /// Lookup a command version by firmware group and command ID.
    // upstream: if_iwx.c iwx_lookup_cmd_ver()
    pub fn lookup_command_version(&self, group: u8, command: u8) -> u8 {
        self.command_versions
            .iter()
            .find(|entry| entry[1] == group && entry[0] == command)
            .map_or(FW_CMD_VER_UNKNOWN, |entry| entry[2])
    }

    /// Lookup a notification version by firmware group and command ID.
    // upstream: if_iwx.c iwx_lookup_notif_ver()
    pub fn lookup_notification_version(&self, group: u8, command: u8) -> u8 {
        self.command_versions
            .iter()
            .find(|entry| entry[1] == group && entry[0] == command)
            .map_or(FW_CMD_VER_UNKNOWN, |entry| entry[3])
    }

    /// Count LMAC/UMAC/paging sections using their firmware separator markers.
    // upstream: if_iwx.c iwx_get_num_sections()
    pub fn section_counts_by_layout(&self) -> (usize, usize, usize) {
        let lmac = count_sections(
            self.sections
                .iter()
                .filter(|section| section.kind == SectionType::Regular),
            0,
        );
        let umac = count_sections(
            self.sections
                .iter()
                .filter(|section| section.kind == SectionType::Regular),
            lmac + 1,
        );
        let paging = count_sections(
            self.sections
                .iter()
                .filter(|section| section.kind == SectionType::Regular),
            lmac + umac + 2,
        );
        (lmac, umac, paging)
    }
}

/// OpenBSD reports the supported stream of HT MCS 8 through MCS 15 as MIMO2.
// upstream: if_iwx.c iwx_is_mimo_ht_plcp()
pub const fn is_mimo_ht_plcp(ht_plcp: u8) -> bool {
    matches!(ht_plcp, 0x8..=0xf)
}

/// Format the firmware version as OpenBSD does, including its API-dependent minor radix.
// upstream: if_iwx.c iwx_fw_version_str()
pub fn firmware_version_string(major: u32, minor: u32, api: u32) -> alloc::string::String {
    if major >= 35 {
        format!("{major}.{minor:08x}.{api}")
    } else {
        format!("{major}.{minor}.{api}")
    }
}

// upstream: if_iwx.c iwx_store_cscheme()
fn validate_cipher_schemes(data: &[u8]) -> Result<(), FirmwareError> {
    let Some(&count) = data.first() else {
        return Err(FirmwareError::InvalidImage);
    };
    let needed = 1usize
        .checked_add(
            (count as usize)
                .checked_mul(FW_CIPHER_SCHEME_SIZE)
                .ok_or(FirmwareError::InvalidImage)?,
        )
        .ok_or(FirmwareError::InvalidImage)?;
    if data.len() < needed {
        return Err(FirmwareError::InvalidImage);
    }
    Ok(())
}

fn count_sections<'a>(sections: impl Iterator<Item = &'a FirmwareSection>, start: usize) -> usize {
    let mut count = 0;
    for (index, section) in sections.enumerate().skip(start) {
        let _ = index;
        if section.device_offset == CPU1_CPU2_SEPARATOR || section.device_offset == PAGING_SEPARATOR
        {
            break;
        }
        count += 1;
    }
    count
}

/// Select the PNVM subsection matching the device SKU and runtime MAC/RF type.
// upstream: if_iwx.c iwx_pnvm_parse()
pub fn select_pnvm(
    bytes: &[u8],
    sku_id: [u32; 3],
    mac_type: u16,
    rf_type: u16,
) -> Result<Option<PnvmImage>, FirmwareError> {
    if sku_id == [0; 3] {
        return Ok(None);
    }

    let mut cursor = 0usize;
    while bytes.len().saturating_sub(cursor) >= 8 {
        let kind = read_u32(bytes, cursor)?;
        let len = read_u32(bytes, cursor + 4)? as usize;
        let data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
        let remaining = bytes.len().saturating_sub(data);
        let padded = len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
        if remaining < len || padded > remaining {
            return Err(FirmwareError::InvalidImage);
        }
        let next = data
            .checked_add(padded)
            .ok_or(FirmwareError::InvalidImage)?;

        if kind == TLV_PNVM_SKU {
            // OpenBSD verifies the TLV extent, then reads the three-word SKU
            // structure from the following bytes without requiring len == 12.
            let sku = bytes
                .get(data..data.checked_add(12).ok_or(FirmwareError::InvalidImage)?)
                .ok_or(FirmwareError::InvalidImage)?;
            let found = [
                u32::from_le_bytes(sku[0..4].try_into().unwrap()),
                u32::from_le_bytes(sku[4..8].try_into().unwrap()),
                u32::from_le_bytes(sku[8..12].try_into().unwrap()),
            ];
            if found == sku_id {
                // iwx_pnvm_handle_section() errors are a section mismatch at
                // this level; continue scanning after this SKU marker.
                if let Ok(Some(selected)) = parse_pnvm_section(&bytes[next..], mac_type, rf_type) {
                    return Ok(Some(selected));
                }
            }
        }
        cursor = next;
    }
    Ok(None)
}

// upstream: if_iwx.c iwx_pnvm_handle_section()
fn parse_pnvm_section(
    bytes: &[u8],
    mac_type: u16,
    rf_type: u16,
) -> Result<Option<PnvmImage>, FirmwareError> {
    let mut cursor = 0usize;
    let mut selected = PnvmImage {
        version: 0,
        total_size: 0,
        segments: Vec::new(),
    };
    let mut hw_match = false;
    while bytes.len().saturating_sub(cursor) >= 8 {
        let kind = read_u32(bytes, cursor)?;
        if kind == TLV_PNVM_SKU {
            break;
        }
        let len = read_u32(bytes, cursor + 4)? as usize;
        let data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
        let remaining = bytes.len().saturating_sub(data);
        if remaining < len {
            return Err(FirmwareError::InvalidImage);
        }
        let end = data.checked_add(len).ok_or(FirmwareError::InvalidImage)?;
        let padded = len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
        let next = data
            .checked_add(padded)
            .ok_or(FirmwareError::InvalidImage)?;
        match kind {
            TLV_PNVM_VERSION if len >= 4 => selected.version = read_u32(bytes, data)?,
            TLV_HW_TYPE if len >= 4 && !hw_match => {
                hw_match =
                    read_u16(bytes, data)? == mac_type && read_u16(bytes, data + 2)? == rf_type;
            }
            TLV_SEC_RT => {
                if len < 4 {
                    return Err(FirmwareError::InvalidImage);
                }
                if read_u32(bytes, data)? != 0xdddd_eeee {
                    if selected.segments.len() >= 64 {
                        return Err(FirmwareError::TooManySections);
                    }
                    let segment = bytes[data + 4..end].to_vec();
                    selected.total_size = selected
                        .total_size
                        .checked_add(segment.len())
                        .ok_or(FirmwareError::InvalidImage)?;
                    selected.segments.push(segment);
                }
            }
            _ => {}
        }
        // OpenBSD treats incomplete trailing alignment padding as the end of
        // this SKU segment, then checks hw_match and total size.
        if padded > remaining {
            break;
        }
        cursor = next;
    }
    if hw_match && selected.total_size != 0 {
        Ok(Some(selected))
    } else {
        Ok(None)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, FirmwareError> {
    let raw = bytes
        .get(offset..offset.checked_add(2).ok_or(FirmwareError::InvalidImage)?)
        .ok_or(FirmwareError::InvalidImage)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, FirmwareError> {
    let raw = bytes
        .get(offset..offset.checked_add(4).ok_or(FirmwareError::InvalidImage)?)
        .ok_or(FirmwareError::InvalidImage)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn read_tlv_u32(data: &[u8]) -> Result<u32, FirmwareError> {
    if data.len() < 4 {
        return Err(FirmwareError::InvalidImage);
    }
    read_u32(data, 0)
}

// upstream: if_iwx.c iwx_firmware_store_section()
fn store_section(
    image: &mut FirmwareImage,
    kind: SectionType,
    data: &[u8],
) -> Result<(), FirmwareError> {
    if data.len() < 4 {
        return Err(FirmwareError::InvalidImage);
    }
    let count = &mut image.section_counts[kind.index()];
    if *count >= SECTION_LIMIT {
        return Err(FirmwareError::TooManySections);
    }
    image.sections.push(FirmwareSection {
        kind,
        device_offset: read_u32(data, 0)?,
        bytes: data[4..].to_vec(),
    });
    *count += 1;
    Ok(())
}

// upstream: if_iwx.c iwx_read_firmware() API_CHANGES_SET and ENABLED_CAPABILITIES cases
fn set_bitmap<const N: usize>(bitmap: &mut [u32; N], data: &[u8]) -> Result<(), FirmwareError> {
    if data.len() != 8 {
        return Err(FirmwareError::InvalidCapabilities);
    }
    let index = read_u32(data, 0)? as usize;
    if index >= N {
        return Err(FirmwareError::InvalidCapabilities);
    }
    bitmap[index] |= read_u32(data, 4)?;
    Ok(())
}

/// Build a valid minimal TLV image for parser unit tests.
#[cfg(test)]
pub(crate) fn test_image(tlvs: &[(u32, &[u8])]) -> Vec<u8> {
    let mut bytes = vec![0; HEADER_LEN];
    bytes[4..8].copy_from_slice(&TLV_MAGIC.to_le_bytes());
    bytes[72..76].copy_from_slice(&((1 << 24) | (2 << 16) | (AX210_UCODE_API << 8)).to_le_bytes());
    for (kind, data) in tlvs {
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        while !bytes.len().is_multiple_of(4) {
            bytes.push(0);
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_up_command_and_notification_versions_and_unknown_default() {
        let commands = [[0x44, 3, 7, 9]];
        let image = FirmwareImage {
            command_versions: commands.to_vec(),
            ..FirmwareImage::parse(&test_image(&[])).unwrap()
        };
        assert_eq!(image.lookup_command_version(3, 0x44), 7);
        assert_eq!(image.lookup_notification_version(3, 0x44), 9);
        assert_eq!(image.lookup_command_version(0, 0), 99);
    }

    #[test]
    fn detects_mimo_ht_rates_and_formats_api_dependent_version_minor() {
        assert!(!is_mimo_ht_plcp(7));
        assert!(is_mimo_ht_plcp(8));
        assert!(is_mimo_ht_plcp(15));
        assert!(!is_mimo_ht_plcp(16));
        assert_eq!(firmware_version_string(34, 13, 89), "34.13.89");
        assert_eq!(firmware_version_string(35, 13, 89), "35.0000000d.89");
    }

    #[test]
    fn splits_firmware_sections_at_lmac_umac_and_paging_separators() {
        let ordinary = 0x1000u32.to_le_bytes();
        let image = FirmwareImage::parse(&test_image(&[
            (TLV_SEC_RT, &ordinary),
            (TLV_SEC_RT, &ordinary),
            (TLV_SEC_RT, &CPU1_CPU2_SEPARATOR.to_le_bytes()),
            (TLV_SEC_RT, &ordinary),
            (TLV_SEC_RT, &PAGING_SEPARATOR.to_le_bytes()),
            (TLV_SEC_RT, &ordinary),
            (TLV_SEC_RT, &ordinary),
        ]))
        .unwrap();
        assert_eq!(image.section_counts_by_layout(), (2, 1, 2));
    }

    #[test]
    fn parses_ax211_image_sections_and_pnvm() {
        let mut fw_section = 0x1234_0000u32.to_le_bytes().to_vec();
        fw_section.extend_from_slice(&[1, 2, 3]);
        let image = FirmwareImage::parse(&test_image(&[
            (TLV_SEC_RT, &fw_section),
            (TLV_FLAGS, &0x1234u32.to_le_bytes()),
            (TLV_PNVM_DATA, &[9, 8, 7]),
        ]))
        .unwrap();
        assert_eq!(image.sections[0].kind, SectionType::Regular);
        assert_eq!(image.sections[0].device_offset, 0x1234_0000);
        assert_eq!(image.sections[0].bytes, [1, 2, 3]);
        assert_eq!(image.capability_flags, 0x1234);
        assert_eq!(image.pnvm.as_deref(), Some(&[9, 8, 7][..]));
    }

    #[test]
    fn selects_pnvm_by_sku_and_hardware() {
        fn tlv(kind: u32, data: &[u8], out: &mut Vec<u8>) {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        let mut pnvm = Vec::new();
        let sku = [4u32, 5, 6]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        tlv(TLV_PNVM_SKU, &sku, &mut pnvm);
        let mut hw = 0x43u16.to_le_bytes().to_vec();
        hw.extend_from_slice(&0x10du16.to_le_bytes());
        tlv(TLV_HW_TYPE, &hw, &mut pnvm);
        tlv(TLV_PNVM_VERSION, &12u32.to_le_bytes(), &mut pnvm);
        let mut section = 0xddddeeeeu32.to_le_bytes().to_vec();
        tlv(TLV_SEC_RT, &section, &mut pnvm); // Deprecated delimiter is ignored.
        section = 0x1000u32.to_le_bytes().to_vec();
        section.extend_from_slice(&[8, 9]);
        tlv(TLV_SEC_RT, &section, &mut pnvm);
        assert_eq!(
            select_pnvm(&pnvm, [4, 5, 6], 0x43, 0x10d).unwrap(),
            Some(PnvmImage {
                version: 12,
                total_size: 2,
                segments: vec![vec![8, 9]]
            })
        );
        assert_eq!(select_pnvm(&pnvm, [0, 0, 0], 0x43, 0x10d).unwrap(), None);
    }

    #[test]
    fn sku_tlv_uses_first_three_words_and_skips_bad_section_for_later_sku() {
        let mut data = Vec::new();
        let sku = [4u32, 5, 6, 0xdead_beef]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        let mut tlv = |kind: u32, payload: &[u8]| {
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            data.extend_from_slice(payload);
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
        };
        tlv(TLV_PNVM_SKU, &sku); // length is 16, but only first 12 bytes are the SKU.
        tlv(TLV_SEC_RT, &[1, 2]); // bad matched subsection is skipped by the outer selector.
        tlv(TLV_PNVM_SKU, &[4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0]);
        let mut hw = 0x43u16.to_le_bytes().to_vec();
        hw.extend_from_slice(&0x10du16.to_le_bytes());
        tlv(TLV_HW_TYPE, &hw);
        let mut section = 0x1000u32.to_le_bytes().to_vec();
        section.extend_from_slice(&[7, 8, 9]);
        tlv(TLV_SEC_RT, &section);
        assert_eq!(
            select_pnvm(&data, [4, 5, 6], 0x43, 0x10d)
                .unwrap()
                .unwrap()
                .total_size,
            3
        );
    }

    #[test]
    fn sku_nested_missing_final_pad_ends_section_after_valid_segments() {
        let mut data = Vec::new();
        data.extend_from_slice(&TLV_PNVM_SKU.to_le_bytes());
        data.extend_from_slice(&12u32.to_le_bytes());
        data.extend_from_slice(&[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0]);
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        let mut hw = Vec::new();
        hw.extend_from_slice(&0x43u16.to_le_bytes());
        hw.extend_from_slice(&0x10du16.to_le_bytes());
        tlv_append(TLV_HW_TYPE, &hw, &mut data);
        let mut section = 0x1234u32.to_le_bytes().to_vec();
        section.push(0xaa); // a single valid byte with absent final three-byte padding
        data.extend_from_slice(&TLV_SEC_RT.to_le_bytes());
        data.extend_from_slice(&(section.len() as u32).to_le_bytes());
        data.extend_from_slice(&section);
        assert_eq!(
            select_pnvm(&data, [1, 2, 3], 0x43, 0x10d)
                .unwrap()
                .unwrap()
                .total_size,
            1
        );
    }

    fn tlv_append(kind: u32, payload: &[u8], data: &mut Vec<u8>) {
        data.extend_from_slice(&kind.to_le_bytes());
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(payload);
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
    }

    #[test]
    fn zero_sku_returns_before_parsing_pnvm() {
        assert_eq!(select_pnvm(&[0xff], [0; 3], 0, 0), Ok(None));
    }

    #[test]
    fn rejects_unsupported_api_version() {
        let mut bytes = test_image(&[]);
        bytes[72..76].copy_from_slice(&((1u32 << 24) | (2 << 16) | (77 << 8)).to_le_bytes());
        assert_eq!(
            FirmwareImage::parse(&bytes),
            Err(FirmwareError::UnsupportedApi(77))
        );
    }

    #[test]
    fn parses_default_calibration_and_counts_sections_by_type() {
        let calibration = [2u32.to_le_bytes(), 3u32.to_le_bytes(), 4u32.to_le_bytes()].concat();
        let section = [0x1000u32.to_le_bytes(), [1, 2, 3, 4]].concat();
        let mut tlvs = Vec::new();
        for _ in 0..SECTION_LIMIT {
            tlvs.push((TLV_SEC_RT, section.as_slice()));
        }
        for _ in 0..SECTION_LIMIT {
            tlvs.push((TLV_SEC_INIT, section.as_slice()));
        }
        tlvs.push((TLV_DEF_CALIB, calibration.as_slice()));
        let bytes = test_image(&tlvs);
        let image = FirmwareImage::parse(&bytes).unwrap();
        assert_eq!(image.section_counts, [SECTION_LIMIT, SECTION_LIMIT, 0, 0]);
        assert_eq!(
            image.default_calibration[2],
            Some(DefaultCalibration {
                flow_trigger: 3,
                event_trigger: 4
            })
        );
        let too_many = test_image(&[(TLV_SEC_RT, section.as_slice()); SECTION_LIMIT + 1]);
        assert_eq!(
            FirmwareImage::parse(&too_many),
            Err(FirmwareError::TooManySections)
        );
    }

    #[test]
    fn validates_cipher_scheme_entries_by_upstream_packed_size() {
        let valid = [1u8]
            .into_iter()
            .chain([0; FW_CIPHER_SCHEME_SIZE])
            .collect::<Vec<_>>();
        assert!(FirmwareImage::parse(&test_image(&[(TLV_CSCHEME, &valid)])).is_ok());
        let invalid = [1u8, 0, 0].to_vec();
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_CSCHEME, &invalid)])),
            Err(FirmwareError::InvalidImage)
        );
    }

    #[test]
    fn unsupported_openbsd_tlvs_remain_errors_and_debug_tlvs_are_retained() {
        for kind in [14, 15, 59] {
            assert_eq!(
                FirmwareImage::parse(&test_image(&[(kind, &[])])),
                Err(FirmwareError::UnsupportedTlv(kind))
            );
        }
        let mut dest = vec![0u8; DBG_DEST_V1_HEADER_LEN];
        dest[0] = 0;
        let conf = [3, 0, 0, 0, 9, 8];
        let image = FirmwareImage::parse(&test_image(&[
            (TLV_FW_DBG_DEST, &dest),
            (TLV_FW_DBG_CONF, &conf),
        ]))
        .unwrap();
        assert_eq!(image.debug_dest, Some(dest));
        assert_eq!(image.debug_configs[3], Some(conf.to_vec()));
        assert!(FirmwareImage::parse(&test_image(&[(0x1000009, &[])])).is_ok());
    }

    #[test]
    fn missing_final_tlv_alignment_pad_is_accepted() {
        let mut bytes = test_image(&[(TLV_FLAGS, &[1, 0, 0, 0])]);
        bytes.extend_from_slice(&TLV_PNVM_DATA.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[4, 5, 6]);
        assert_eq!(
            FirmwareImage::parse(&bytes).unwrap().pnvm,
            Some(vec![4, 5, 6])
        );
    }

    #[test]
    fn rejects_truncated_tlvs_and_invalid_section_prefix() {
        let mut truncated = test_image(&[]);
        truncated.extend_from_slice(&TLV_SEC_RT.to_le_bytes());
        truncated.extend_from_slice(&16u32.to_le_bytes());
        assert_eq!(
            FirmwareImage::parse(&truncated),
            Err(FirmwareError::InvalidImage)
        );
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_SEC_RT, &[1, 2, 3])])),
            Err(FirmwareError::InvalidImage)
        );
    }

    #[test]
    fn retains_command_version_entries_and_checks_debug_destination_version() {
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_CMD_VERSIONS, &[1, 2, 3, 4, 5])]))
                .unwrap()
                .command_versions,
            vec![[1, 2, 3, 4]]
        );
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_FW_DBG_DEST, &[1])])),
            Err(FirmwareError::InvalidImage)
        );
    }

    #[test]
    fn validates_scan_and_api_bitmap_bounds() {
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_N_SCAN_CHANNELS, &68u32.to_le_bytes())])),
            Err(FirmwareError::InvalidScanChannels)
        );
        let api = [4u32.to_le_bytes(), 0u32.to_le_bytes()].concat();
        assert_eq!(
            FirmwareImage::parse(&test_image(&[(TLV_API_CHANGES_SET, &api)])),
            Err(FirmwareError::InvalidCapabilities)
        );
    }
}
