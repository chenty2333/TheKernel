//! PHY-context command layouts from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CommandError, EncodedCommand, HostCommand, vht_control_position};

pub const PHY_CONTEXT_COMMAND: u32 = 0x08;
pub const PHY_BAND_5GHZ: u8 = 0;
pub const PHY_BAND_24GHZ: u8 = 1;
pub const PHY_WIDTH_20: u8 = 0;
pub const PHY_WIDTH_40: u8 = 1;
pub const PHY_WIDTH_80: u8 = 2;
pub const PHY_WIDTH_160: u8 = 3;
pub const PHY_RX_CHAIN_VALID_SHIFT: u32 = 1;
pub const PHY_RX_CHAIN_COUNT_SHIFT: u32 = 10;
pub const PHY_RX_CHAIN_MIMO_COUNT_SHIFT: u32 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhyContextError {
    UnsupportedCommandVersion(u8),
    AllocationFailed,
    Command(CommandError),
}

impl From<CommandError> for PhyContextError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhyContextConfig {
    pub id_and_color: u32,
    pub action: u32,
    pub channel: u8,
    pub is_24ghz: bool,
    pub cdb_supported: bool,
    pub ultra_high_band_channels: bool,
    pub channel_40mhz: bool,
    pub sco: u8,
    pub vht_width: u8,
    pub primary_channel_index: i16,
    pub center_channel_index: i16,
    pub static_chains: u8,
    pub dynamic_chains: u8,
    pub valid_rx_antennas: u8,
    pub rlc_command_version: u8,
    pub command_version: u8,
}

/// Build PHY_CONTEXT_CMD v3/v4, including the larger UHB channel-info layout.
// upstream: if_iwx.c iwx_phy_ctxt_cmd()
pub fn phy_context_command(
    config: PhyContextConfig,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, PhyContextError> {
    if config.command_version != 3 && config.command_version != 4 {
        return Err(PhyContextError::UnsupportedCommandVersion(
            config.command_version,
        ));
    }
    let width = match config.vht_width {
        3 => PHY_WIDTH_160,
        2 => PHY_WIDTH_80,
        _ if config.channel_40mhz && config.sco != 0 => PHY_WIDTH_40,
        _ => PHY_WIDTH_20,
    };
    let ctrl_pos = match width {
        PHY_WIDTH_160 | PHY_WIDTH_80 => {
            vht_control_position(config.primary_channel_index, config.center_channel_index)
        }
        PHY_WIDTH_40 if config.sco == 1 => 0,
        PHY_WIDTH_40 if config.sco == 2 => 4,
        _ => 0,
    };
    let lmac = if config.is_24ghz || !config.cdb_supported {
        0u32
    } else {
        1u32
    };
    let rxchain_info = if config.rlc_command_version == 2 {
        0
    } else {
        (u32::from(config.valid_rx_antennas) << PHY_RX_CHAIN_VALID_SHIFT)
            | (u32::from(config.static_chains) << PHY_RX_CHAIN_COUNT_SHIFT)
            | (u32::from(config.dynamic_chains) << PHY_RX_CHAIN_MIMO_COUNT_SHIFT)
    };
    let payload_size = if config.ultra_high_band_channels {
        32
    } else {
        28
    };
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(payload_size)
        .map_err(|_| PhyContextError::AllocationFailed)?;
    payload.resize(payload_size, 0);
    payload[0..4].copy_from_slice(&config.id_and_color.to_le_bytes());
    payload[4..8].copy_from_slice(&config.action.to_le_bytes());
    if config.ultra_high_band_channels {
        payload[8..12].copy_from_slice(&u32::from(config.channel).to_le_bytes());
        payload[12] = if config.is_24ghz {
            PHY_BAND_24GHZ
        } else {
            PHY_BAND_5GHZ
        };
        payload[13] = width;
        payload[14] = ctrl_pos;
        payload[16..20].copy_from_slice(&lmac.to_le_bytes());
        payload[20..24].copy_from_slice(&rxchain_info.to_le_bytes());
    } else {
        payload[8] = if config.is_24ghz {
            PHY_BAND_24GHZ
        } else {
            PHY_BAND_5GHZ
        };
        payload[9] = config.channel;
        payload[10] = width;
        payload[11] = ctrl_pos;
        payload[12..16].copy_from_slice(&lmac.to_le_bytes());
        payload[16..20].copy_from_slice(&rxchain_info.to_le_bytes());
    }
    let command = HostCommand {
        id: PHY_CONTEXT_COMMAND,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{command::LONG_GROUP, command_group_id};

    fn config() -> PhyContextConfig {
        PhyContextConfig {
            id_and_color: 0x1122_3344,
            action: 2,
            channel: 44,
            is_24ghz: false,
            cdb_supported: true,
            ultra_high_band_channels: false,
            channel_40mhz: true,
            sco: 1,
            vht_width: 2,
            primary_channel_index: 40,
            center_channel_index: 50,
            static_chains: 1,
            dynamic_chains: 2,
            valid_rx_antennas: 3,
            rlc_command_version: 1,
            command_version: 3,
        }
    }

    #[test]
    fn phy_context_serializes_channel_lmac_rxchain_and_control_position() {
        let command = phy_context_command(config(), 3, 0).unwrap();
        assert_eq!(
            command.wire_id,
            (u32::from(LONG_GROUP) << 8) | PHY_CONTEXT_COMMAND
        );
        assert_eq!(command.bytes.len(), 36);
        assert_eq!(&command.bytes[8..12], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&command.bytes[16..20], &[0, 44, PHY_WIDTH_80, 2]);
        assert_eq!(&command.bytes[20..24], &1u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(command.bytes[24..28].try_into().unwrap()),
            (3 << PHY_RX_CHAIN_VALID_SHIFT)
                | (1 << PHY_RX_CHAIN_COUNT_SHIFT)
                | (2 << PHY_RX_CHAIN_MIMO_COUNT_SHIFT)
        );
    }

    #[test]
    fn uhb_phy_context_uses_wide_channel_field_and_rlc_v2_omits_rx_chain() {
        let mut config = config();
        config.ultra_high_band_channels = true;
        config.rlc_command_version = 2;
        config.command_version = 4;
        let command = phy_context_command(config, 0, 0).unwrap();
        assert_eq!(command.bytes.len(), 40);
        assert_eq!(&command.bytes[16..20], &44u32.to_le_bytes());
        assert_eq!(command.bytes[20], PHY_BAND_5GHZ);
        assert_eq!(command.bytes[21], PHY_WIDTH_80);
        assert_eq!(command.bytes[22], 2);
        assert_eq!(&command.bytes[28..32], &[0; 4]);
        assert_eq!(command_group_id(command.wire_id), LONG_GROUP);
    }
}
