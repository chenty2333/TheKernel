//! TLV firmware image parsing translated from OpenBSD iwx.
//!
//! Upstream: OpenBSD `sys/dev/pci/if_iwx.c` revision 1.230, functions
//! `iwx_read_firmware()` and `iwx_firmware_store_section()`; header and TLV
//! layouts from `sys/dev/pci/if_iwxreg.h`. ISC.
//! Copyright (c) 2014, 2016 genua gmbh <info@genua.de>
//!   Author: Stefan Sperling <stsp@openbsd.org>
//! Copyright (c) 2014 Fixup Software Ltd.
//! Copyright (c) 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>

use alloc::vec::Vec;

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
const TLV_HW_TYPE: u32 = 58;
const TLV_PNVM_VERSION: u32 = 62;
const TLV_PNVM_SKU: u32 = 64;
const TLV_CSCHEME: u32 = 28;
const TLV_NUM_OF_CPU: u32 = 27;
const TLV_PAGING: u32 = 32;
const TLV_CMD_VERSIONS: u32 = 48;
const TLV_IML: u32 = 52;
const TLV_PAN_FLAG: u32 = 1;

/// Firmware microcode section type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionType {
    Regular,
    Init,
    Wowlan,
    RegularUsniffer,
}

/// One loadable microcode section, including the device load address prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareSection {
    pub kind: SectionType,
    pub device_offset: u32,
    pub bytes: Vec<u8>,
}

/// Parsed firmware facts used to configure and stage the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareImage {
    pub version: [u32; 3],
    pub sections: Vec<FirmwareSection>,
    pub capability_flags: u32,
    pub probe_max_len: u32,
    pub scan_channels: u32,
    pub phy_config: Option<u32>,
    pub api: [u32; 4],
    pub enabled_capabilities: [u32; 5],
    pub pnvm: Option<Vec<u8>>,
    pub iml: Option<Vec<u8>>,
}

/// The selected PNVM SKU and firmware segments for a hardware RF identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PnvmImage {
    pub version: u32,
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
            capability_flags: 0,
            probe_max_len: 0,
            scan_channels: DEFAULT_SCAN_CHANNELS,
            phy_config: None,
            api: [0; 4],
            enabled_capabilities: [0; 5],
            pnvm: None,
            iml: None,
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
                TLV_CSCHEME => {
                    let count = *data.first().ok_or(FirmwareError::InvalidImage)? as usize;
                    let minimum = 1usize
                        .checked_add(count.checked_mul(2).ok_or(FirmwareError::InvalidImage)?)
                        .ok_or(FirmwareError::InvalidImage)?;
                    if data.len() < minimum {
                        return Err(FirmwareError::InvalidImage);
                    }
                }
                TLV_NUM_OF_CPU => {
                    let cpus = read_tlv_u32(data)?;
                    if data.len() != 4 || !(1..=2).contains(&cpus) {
                        return Err(FirmwareError::InvalidImage);
                    }
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
                    // entry down, then bounds the stored array.
                    if data.len() / 4 > 256 {
                        return Err(FirmwareError::InvalidImage);
                    }
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
                14
                | 15
                | 35
                | 50
                | 51
                | 57
                | 58
                | 59
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
                | 0x1000002..=0x1000005
                | 0x100000b
                | 0x100000c
                | 0x101
                | 1092 => {}
                _ => return Err(FirmwareError::UnsupportedTlv(kind)),
            }
            let padded = len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
            let padding = padded - len;
            if padding > bytes.len().saturating_sub(data_end) {
                // OpenBSD accepts a missing final alignment pad.
                if data_end == bytes.len() {
                    break;
                }
                return Err(FirmwareError::InvalidImage);
            }
            cursor = data_end + padding;
        }
        Ok(image)
    }
}

/// Select the PNVM subsection matching the device SKU and runtime MAC/RF type.
// upstream: if_iwx.c iwx_pnvm_parse()
pub fn select_pnvm(
    bytes: &[u8],
    sku_id: [u32; 3],
    mac_type: u16,
    rf_type: u16,
) -> Result<Option<PnvmImage>, FirmwareError> {
    let mut cursor = 0usize;
    while bytes.len().saturating_sub(cursor) >= 8 {
        let kind = read_u32(bytes, cursor)?;
        let len = read_u32(bytes, cursor + 4)? as usize;
        let data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
        let end = data.checked_add(len).ok_or(FirmwareError::InvalidImage)?;
        if end > bytes.len() {
            return Err(FirmwareError::InvalidImage);
        }
        let padded = len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
        let next = data
            .checked_add(padded)
            .ok_or(FirmwareError::InvalidImage)?;
        if next > bytes.len() {
            return Err(FirmwareError::InvalidImage);
        }
        if kind == TLV_PNVM_SKU {
            if len != 12 {
                return Err(FirmwareError::InvalidImage);
            }
            let found = [
                read_u32(bytes, data)?,
                read_u32(bytes, data + 4)?,
                read_u32(bytes, data + 8)?,
            ];
            let section_start = next;
            cursor = next;
            if found == sku_id {
                let mut selected = PnvmImage {
                    version: 0,
                    segments: Vec::new(),
                };
                let mut hw_match = false;
                while bytes.len().saturating_sub(cursor) >= 8 {
                    let sub_kind = read_u32(bytes, cursor)?;
                    if sub_kind == TLV_PNVM_SKU {
                        break;
                    }
                    let sub_len = read_u32(bytes, cursor + 4)? as usize;
                    let sub_data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
                    let sub_end = sub_data
                        .checked_add(sub_len)
                        .ok_or(FirmwareError::InvalidImage)?;
                    if sub_end > bytes.len() {
                        return Err(FirmwareError::InvalidImage);
                    }
                    let sub_padded =
                        sub_len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
                    let sub_next = sub_data
                        .checked_add(sub_padded)
                        .ok_or(FirmwareError::InvalidImage)?;
                    if sub_next > bytes.len() {
                        return Err(FirmwareError::InvalidImage);
                    }
                    match sub_kind {
                        TLV_PNVM_VERSION if sub_len >= 4 => {
                            selected.version = read_u32(bytes, sub_data)?;
                        }
                        TLV_HW_TYPE if sub_len >= 4 && !hw_match => {
                            let found_mac = read_u16(bytes, sub_data)?;
                            let found_rf = read_u16(bytes, sub_data + 2)?;
                            hw_match = found_mac == mac_type && found_rf == rf_type;
                        }
                        TLV_SEC_RT => {
                            if sub_len < 4 {
                                return Err(FirmwareError::InvalidImage);
                            }
                            if read_u32(bytes, sub_data)? != 0xdddd_eeee {
                                if selected.segments.len() >= 64 {
                                    return Err(FirmwareError::TooManySections);
                                }
                                selected
                                    .segments
                                    .push(bytes[sub_data + 4..sub_end].to_vec());
                            }
                        }
                        _ => {}
                    }
                    cursor = sub_next;
                }
                if hw_match && !selected.segments.is_empty() {
                    return Ok(Some(selected));
                }
                // If the matching SKU had no matching hardware, continue at
                // its next SKU header, not at the end of the file.
                cursor = section_start;
                while bytes.len().saturating_sub(cursor) >= 8 {
                    let sub_kind = read_u32(bytes, cursor)?;
                    if sub_kind == TLV_PNVM_SKU {
                        break;
                    }
                    let sub_len = read_u32(bytes, cursor + 4)? as usize;
                    let sub_data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
                    let sub_padded =
                        sub_len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3;
                    cursor = sub_data
                        .checked_add(sub_padded)
                        .ok_or(FirmwareError::InvalidImage)?;
                    if cursor > bytes.len() {
                        return Err(FirmwareError::InvalidImage);
                    }
                }
            } else {
                cursor = next;
                while bytes.len().saturating_sub(cursor) >= 8 {
                    if read_u32(bytes, cursor)? == TLV_PNVM_SKU {
                        break;
                    }
                    let sub_len = read_u32(bytes, cursor + 4)? as usize;
                    let sub_data = cursor.checked_add(8).ok_or(FirmwareError::InvalidImage)?;
                    cursor = sub_data
                        .checked_add(
                            sub_len.checked_add(3).ok_or(FirmwareError::InvalidImage)? & !3,
                        )
                        .ok_or(FirmwareError::InvalidImage)?;
                    if cursor > bytes.len() {
                        return Err(FirmwareError::InvalidImage);
                    }
                }
            }
        } else {
            cursor = next;
        }
    }
    Ok(None)
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
    if image.sections.len() >= SECTION_LIMIT {
        return Err(FirmwareError::TooManySections);
    }
    image.sections.push(FirmwareSection {
        kind,
        device_offset: read_u32(data, 0)?,
        bytes: data[4..].to_vec(),
    });
    Ok(())
}

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
fn test_image(tlvs: &[(u32, &[u8])]) -> Vec<u8> {
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
                segments: vec![vec![8, 9]]
            })
        );
        assert_eq!(select_pnvm(&pnvm, [0, 0, 0], 0x43, 0x10d).unwrap(), None);
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
