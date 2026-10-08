//! RX metadata helpers translated from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::DeviceFamily;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxMetadataError {
    DescriptorTooShort,
}

pub const RX_PHY_INFO_BYTES: usize = 68;

/// Cached firmware PHY report associated with the following RX MPDU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxPhyInfo {
    pub non_configurable_count: u8,
    pub configurable_count: u8,
    pub status_id: u8,
    pub system_timestamp: u32,
    pub timestamp: u64,
    pub beacon_timestamp: u32,
    pub phy_flags: u16,
    pub channel: u16,
    pub non_configurable_phy: [u32; 8],
    pub rate_n_flags: u32,
    pub byte_count: u32,
    pub mac_active_mask: u16,
    pub frame_time: u16,
}

/// Copy a checked RX_PHY notification into the driver-side cache.
// upstream: if_iwx.c iwx_rx_rx_phy_cmd()
pub fn parse_rx_phy_info(payload: &[u8]) -> Result<RxPhyInfo, RxMetadataError> {
    if payload.len() < RX_PHY_INFO_BYTES {
        return Err(RxMetadataError::DescriptorTooShort);
    }
    let mut non_configurable_phy = [0; 8];
    for (index, value) in non_configurable_phy.iter_mut().enumerate() {
        *value = read_u32(payload, 24 + index * 4);
    }
    Ok(RxPhyInfo {
        non_configurable_count: payload[0],
        configurable_count: payload[1],
        status_id: payload[2],
        system_timestamp: read_u32(payload, 4),
        timestamp: u64::from_le_bytes(payload[8..16].try_into().unwrap()),
        beacon_timestamp: read_u32(payload, 16),
        phy_flags: read_u16(payload, 20),
        channel: read_u16(payload, 22),
        non_configurable_phy,
        rate_n_flags: read_u32(payload, 56),
        byte_count: read_u32(payload, 60),
        mac_active_mask: read_u16(payload, 64),
        frame_time: read_u16(payload, 66),
    })
}

const fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

const fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Extract the better (less negative) antenna energy from an RX MPDU descriptor.
// upstream: if_iwx.c iwx_rxmq_get_signal_strength()
pub fn signal_strength_dbm(
    family: DeviceFamily,
    descriptor: &[u8],
) -> Result<i16, RxMetadataError> {
    let energy_offset = if family >= DeviceFamily::Ax210 {
        20 // absolute offset from iwx_rx_mpdu_desc start (v3)
    } else {
        12 // RX_MPDU_RES_START_API_S_VER_1 descriptor v1
    };
    let energies = descriptor
        .get(energy_offset..energy_offset + 2)
        .ok_or(RxMetadataError::DescriptorTooShort)?;
    let energy_a = if energies[0] != 0 {
        -(energies[0] as i16)
    } else {
        -256
    };
    let energy_b = if energies[1] != 0 {
        -(energies[1] as i16)
    } else {
        -256
    };
    Ok(energy_a.max(energy_b))
}

/// Average nonzero beacon-silence RSSI values and convert to dBm.
// upstream: if_iwx.c iwx_get_noise()
pub fn noise_dbm(beacon_silence_rssi: [u32; 3]) -> i16 {
    let mut total = 0u32;
    let mut count = 0u32;
    for value in beacon_silence_rssi {
        let noise = u32::from_le(value) & 0xff;
        if noise != 0 {
            total += noise;
            count += 1;
        }
    }
    if count == 0 {
        -127
    } else {
        (total / count) as i16 - 107
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_energy_offsets_follow_descriptor_generation() {
        let mut desc = [0; 24];
        desc[12] = 51;
        desc[13] = 42;
        desc[20] = 30;
        desc[21] = 40;
        assert_eq!(
            signal_strength_dbm(DeviceFamily::Family22000, &desc),
            Ok(-42)
        );
        assert_eq!(signal_strength_dbm(DeviceFamily::Ax210, &desc), Ok(-30));
        assert_eq!(
            signal_strength_dbm(DeviceFamily::Ax210, &desc[..21]),
            Err(RxMetadataError::DescriptorTooShort)
        );
    }

    #[test]
    fn noise_ignores_empty_antennas_and_uses_minus_127_fallback() {
        assert_eq!(noise_dbm([0, 0, 0]), -127);
        assert_eq!(noise_dbm([120, 0, u32::from_le(130)]), 18);
    }

    #[test]
    fn rx_phy_report_preserves_little_endian_fields_and_rejects_short_input() {
        let mut payload = [0u8; RX_PHY_INFO_BYTES];
        payload[0] = 8;
        payload[1] = 2;
        payload[2] = 3;
        payload[4..8].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        payload[8..16].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        payload[16..20].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        payload[20..22].copy_from_slice(&0x0204u16.to_le_bytes());
        payload[22..24].copy_from_slice(&149u16.to_le_bytes());
        payload[24..28].copy_from_slice(&0xaabb_ccddu32.to_le_bytes());
        payload[56..60].copy_from_slice(&0x8765_4321u32.to_le_bytes());
        payload[60..64].copy_from_slice(&1500u32.to_le_bytes());
        payload[64..66].copy_from_slice(&3u16.to_le_bytes());
        payload[66..68].copy_from_slice(&42u16.to_le_bytes());
        let parsed = parse_rx_phy_info(&payload).unwrap();
        assert_eq!(parsed.non_configurable_count, 8);
        assert_eq!(parsed.timestamp, 0x1122_3344_5566_7788);
        assert_eq!(parsed.channel, 149);
        assert_eq!(parsed.non_configurable_phy[0], 0xaabb_ccdd);
        assert_eq!(parsed.rate_n_flags, 0x8765_4321);
        assert_eq!(parsed.byte_count, 1500);
        assert_eq!(parsed.frame_time, 42);
        assert_eq!(
            parse_rx_phy_info(&payload[..67]),
            Err(RxMetadataError::DescriptorTooShort)
        );
    }
}
