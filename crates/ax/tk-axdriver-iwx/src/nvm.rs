//! NVM_GET_INFO request and NVM response extraction from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{
    CMD_SEND_DURING_RFKILL, CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand,
    MacAddress, is_valid_mac_address,
};

pub const REGULATORY_AND_NVM_GROUP: u8 = 0x0c;
pub const NVM_GET_INFO_CMD: u8 = 0x02;
pub const NVM_V3_CHANNEL_COUNT: usize = 51;
pub const NVM_V4_CHANNEL_COUNT: usize = 110;
pub const NVM_V3_RESPONSE_BYTES: usize = 128;
pub const NVM_V4_RESPONSE_BYTES: usize = 468;
pub const NVM_CHANNEL_VALID: u32 = 1 << 0;
pub const NVM_CHANNEL_ACTIVE: u32 = 1 << 3;
pub const NVM_CHANNEL_40MHZ: u32 = 1 << 9;
pub const NVM_CHANNEL_80MHZ: u32 = 1 << 10;
pub const NVM_CHANNEL_160MHZ: u32 = 1 << 11;
const MAC_SKU_BAND_24: u32 = 1 << 0;
const MAC_SKU_BAND_52: u32 = 1 << 1;
const MAC_SKU_11N: u32 = 1 << 2;
const MAC_SKU_11AC: u32 = 1 << 3;
const MAC_SKU_11AX: u32 = 1 << 4;
const MAC_SKU_MIMO_DISABLED: u32 = 1 << 5;
const GENERAL_EMPTY_OTP: u32 = 1;
const LAR_CAPABILITY: u32 = 1 << 0;

/// NVM data produced by `iwx_nvm_get()` for channel and link configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NvmInfo {
    pub hardware_address: MacAddress,
    pub nvm_version: u16,
    pub board_type: u8,
    pub hardware_address_count: u8,
    pub empty_otp: bool,
    pub band_24ghz: bool,
    pub band_52ghz: bool,
    pub supports_11n: bool,
    pub supports_11ac: bool,
    pub supports_11ax: bool,
    pub mimo_disabled: bool,
    pub valid_tx_antennas: u8,
    pub valid_rx_antennas: u8,
    pub lar_enabled: bool,
    pub channel_profiles: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NvmError {
    InvalidLength,
    InvalidMacAddress,
    AllocationFailed,
}

/// Encode an NVM request, permitting it while hardware RF-kill is active.
// upstream: if_iwx.c iwx_nvm_get() host command setup
pub fn nvm_get_command(
    regulatory_v4: bool,
    slot: u8,
    command_queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let request = [0u8; 4];
    let response_bytes = if regulatory_v4 {
        NVM_V4_RESPONSE_BYTES
    } else {
        NVM_V3_RESPONSE_BYTES
    };
    let command = HostCommand {
        id: (u32::from(REGULATORY_AND_NVM_GROUP) << 8) | u32::from(NVM_GET_INFO_CMD),
        flags: CMD_WANT_RESPONSE | CMD_SEND_DURING_RFKILL,
        response_capacity: 8 + response_bytes,
        parts: &[&request],
    };
    EncodedCommand::encode(&command, slot, command_queue)
}

/// Decode v3/v4 regulatory NVM payload and combine SKU, antennas, MAC and LAR.
// upstream: if_iwx.c iwx_nvm_get()
pub fn parse_nvm_response(
    response: &[u8],
    regulatory_v4: bool,
    hardware_address: Option<MacAddress>,
    enabled_capabilities: &[u32],
) -> Result<NvmInfo, NvmError> {
    let (expected, channels, profile_offset, profile_bytes) = if regulatory_v4 {
        (NVM_V4_RESPONSE_BYTES, NVM_V4_CHANNEL_COUNT, 28, 4)
    } else {
        (NVM_V3_RESPONSE_BYTES, NVM_V3_CHANNEL_COUNT, 24, 2)
    };
    if response.len() != expected {
        return Err(NvmError::InvalidLength);
    }
    let hardware_address = hardware_address.ok_or(NvmError::InvalidMacAddress)?;
    if !is_valid_mac_address(&hardware_address) {
        return Err(NvmError::InvalidMacAddress);
    }
    let general_flags = read_u32(response, 0)?;
    let mac_flags = read_u32(response, 8)?;
    let lar_reported = read_u32(response, 20)? != 0;
    let mut channel_profiles = Vec::new();
    channel_profiles
        .try_reserve_exact(channels)
        .map_err(|_| NvmError::AllocationFailed)?;
    for index in 0..channels {
        let offset = profile_offset + index * profile_bytes;
        let flags = if profile_bytes == 2 {
            u32::from(read_u16(response, offset)?)
        } else {
            read_u32(response, offset)?
        };
        channel_profiles.push(flags);
    }
    Ok(NvmInfo {
        hardware_address,
        nvm_version: read_u16(response, 4)?,
        board_type: response[6],
        hardware_address_count: response[7],
        empty_otp: general_flags & GENERAL_EMPTY_OTP != 0,
        band_24ghz: mac_flags & MAC_SKU_BAND_24 != 0,
        band_52ghz: mac_flags & MAC_SKU_BAND_52 != 0,
        supports_11n: mac_flags & MAC_SKU_11N != 0,
        supports_11ac: mac_flags & MAC_SKU_11AC != 0,
        supports_11ax: mac_flags & MAC_SKU_11AX != 0,
        mimo_disabled: mac_flags & MAC_SKU_MIMO_DISABLED != 0,
        valid_tx_antennas: response[12],
        valid_rx_antennas: response[16],
        lar_enabled: lar_reported
            && enabled_capabilities
                .iter()
                .any(|capability| capability & LAR_CAPABILITY != 0),
        channel_profiles,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, NvmError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or(NvmError::InvalidLength)?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, NvmError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or(NvmError::InvalidLength)?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn nvm_request_has_nvm_group_and_rfkill_response_flags() {
        let command = nvm_get_command(true, 4, 0).unwrap();
        assert_eq!(
            command.wire_id,
            (u32::from(REGULATORY_AND_NVM_GROUP) << 8) | u32::from(NVM_GET_INFO_CMD)
        );
        assert_eq!(command.flags, CMD_WANT_RESPONSE | CMD_SEND_DURING_RFKILL);
        assert_eq!(command.response_capacity, 8 + NVM_V4_RESPONSE_BYTES);
        assert_eq!(&command.bytes[8..], &[0; 4]);
    }

    #[test]
    fn nvm_v3_and_v4_profiles_are_decoded_with_capability_gated_lar() {
        let mut v3 = vec![0; NVM_V3_RESPONSE_BYTES];
        v3[0..4].copy_from_slice(&GENERAL_EMPTY_OTP.to_le_bytes());
        v3[4..6].copy_from_slice(&0x1234u16.to_le_bytes());
        v3[6] = 0x56;
        v3[7] = 2;
        v3[8..12].copy_from_slice(&(MAC_SKU_BAND_24 | MAC_SKU_11N | MAC_SKU_11AX).to_le_bytes());
        v3[12..16].copy_from_slice(&3u32.to_le_bytes());
        v3[16..20].copy_from_slice(&1u32.to_le_bytes());
        v3[20..24].copy_from_slice(&1u32.to_le_bytes());
        v3[24..26].copy_from_slice(&(NVM_CHANNEL_VALID | NVM_CHANNEL_ACTIVE).to_le_bytes()[..2]);
        let valid_mac = [0, 1, 2, 3, 4, 5];
        let parsed = parse_nvm_response(&v3, false, Some(valid_mac), &[LAR_CAPABILITY]).unwrap();
        assert_eq!(parsed.nvm_version, 0x1234);
        assert_eq!(parsed.board_type, 0x56);
        assert_eq!(parsed.hardware_address_count, 2);
        assert!(parsed.empty_otp);
        assert!(parsed.band_24ghz && parsed.supports_11n && parsed.supports_11ax);
        assert_eq!(parsed.valid_tx_antennas, 3);
        assert_eq!(parsed.valid_rx_antennas, 1);
        assert!(parsed.lar_enabled);
        assert_eq!(
            parsed.channel_profiles[0],
            NVM_CHANNEL_VALID | NVM_CHANNEL_ACTIVE
        );

        let mut v4 = vec![0; NVM_V4_RESPONSE_BYTES];
        v4[20..24].copy_from_slice(&1u32.to_le_bytes());
        v4[24..28].copy_from_slice(&1u32.to_le_bytes());
        v4[28..32].copy_from_slice(&(NVM_CHANNEL_VALID | NVM_CHANNEL_80MHZ).to_le_bytes());
        let parsed = parse_nvm_response(&v4, true, Some(valid_mac), &[]).unwrap();
        assert!(!parsed.lar_enabled);
        assert_eq!(parsed.channel_profiles.len(), NVM_V4_CHANNEL_COUNT);
        assert_eq!(
            parsed.channel_profiles[0],
            NVM_CHANNEL_VALID | NVM_CHANNEL_80MHZ
        );
    }

    #[test]
    fn nvm_response_requires_expected_payload_and_valid_mac() {
        assert_eq!(
            parse_nvm_response(&[], false, Some([0; 6]), &[]),
            Err(NvmError::InvalidLength)
        );
        assert_eq!(
            parse_nvm_response(
                &vec![0; NVM_V3_RESPONSE_BYTES],
                false,
                Some([1, 2, 3, 4, 5, 6]),
                &[]
            ),
            Err(NvmError::InvalidMacAddress)
        );
    }
}
