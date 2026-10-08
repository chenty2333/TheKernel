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
pub const RLC_CONFIG_COMMAND: u8 = 0x08;
pub const RLC_CONFIG_VERSION: u8 = 2;
pub const PHY_CONTEXT_ACTION_ADD: u32 = 1;
pub const PHY_CONTEXT_ACTION_MODIFY: u32 = 2;
pub const PHY_CONTEXT_ACTION_REMOVE: u32 = 3;

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

/// Build API-v2 RLC_CONFIG with valid/static/dynamic receive-chain fields.
// upstream: if_iwx.c iwx_phy_send_rlc()
pub fn rlc_config_command(
    phy_id: u32,
    valid_rx_antennas: u8,
    chains_static: u8,
    chains_dynamic: u8,
    slot: u8,
) -> Result<EncodedCommand, PhyContextError> {
    let chain_info = (u32::from(valid_rx_antennas) << PHY_RX_CHAIN_VALID_SHIFT)
        | (u32::from(chains_static) << PHY_RX_CHAIN_COUNT_SHIFT)
        | (u32::from(chains_dynamic) << PHY_RX_CHAIN_MIMO_COUNT_SHIFT);
    let mut payload = [0u8; 32];
    payload[0..4].copy_from_slice(&phy_id.to_le_bytes());
    payload[4..8].copy_from_slice(&chain_info.to_le_bytes());
    let command = HostCommand {
        id: (u32::from(RLC_CONFIG_VERSION) << 16)
            | (u32::from(crate::DATA_PATH_GROUP) << 8)
            | u32::from(RLC_CONFIG_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhyUpdateStage {
    RemoveOld,
    AddNew,
    Modify,
    Rlc,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhyUpdateError<E> {
    Build(PhyContextError),
    Send { stage: PhyUpdateStage, error: E },
}

/// Apply the source remove/add on cross-band CDB transition, else modify in place.
// upstream: if_iwx.c iwx_phy_ctxt_update()
pub fn update_phy_context<E>(
    current: &mut PhyContextConfig,
    mut next: PhyContextConfig,
    cdb_supported: bool,
    slot: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), PhyUpdateError<E>> {
    let cross_band = (current.is_24ghz != next.is_24ghz) && cdb_supported;
    if cross_band {
        let mut remove = *current;
        remove.action = PHY_CONTEXT_ACTION_REMOVE;
        let command = phy_context_command(remove, slot, 0).map_err(PhyUpdateError::Build)?;
        send(&command).map_err(|error| PhyUpdateError::Send {
            stage: PhyUpdateStage::RemoveOld,
            error,
        })?;
        current.channel = next.channel;
        current.is_24ghz = next.is_24ghz;
        next.action = PHY_CONTEXT_ACTION_ADD;
        let command = phy_context_command(next, slot, 0).map_err(PhyUpdateError::Build)?;
        send(&command).map_err(|error| PhyUpdateError::Send {
            stage: PhyUpdateStage::AddNew,
            error,
        })?;
    } else {
        current.channel = next.channel;
        current.is_24ghz = next.is_24ghz;
        next.action = PHY_CONTEXT_ACTION_MODIFY;
        let command = phy_context_command(next, slot, 0).map_err(PhyUpdateError::Build)?;
        send(&command).map_err(|error| PhyUpdateError::Send {
            stage: PhyUpdateStage::Modify,
            error,
        })?;
    }
    current.sco = next.sco;
    current.vht_width = next.vht_width;
    if next.rlc_command_version == RLC_CONFIG_VERSION {
        let command = rlc_config_command(
            next.id_and_color & 0xff,
            next.valid_rx_antennas,
            next.static_chains,
            next.dynamic_chains,
            slot,
        )
        .map_err(PhyUpdateError::Build)?;
        send(&command).map_err(|error| PhyUpdateError::Send {
            stage: PhyUpdateStage::Rlc,
            error,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

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

    #[test]
    fn rlc_chain_update_and_cross_band_phy_change_keep_source_order() {
        let rlc = rlc_config_command(2, 3, 1, 2, 4).unwrap();
        assert_eq!(
            rlc.wire_id,
            (2 << 16) | (5 << 8) | RLC_CONFIG_COMMAND as u32
        );
        assert_eq!(rlc.bytes.len(), 8 + 32);
        assert_eq!(&rlc.bytes[8..12], &2u32.to_le_bytes());
        assert_eq!(
            u32::from_le_bytes(rlc.bytes[12..16].try_into().unwrap()),
            (3 << PHY_RX_CHAIN_VALID_SHIFT)
                | (1 << PHY_RX_CHAIN_COUNT_SHIFT)
                | (2 << PHY_RX_CHAIN_MIMO_COUNT_SHIFT)
        );

        let mut current = config();
        let mut next = current;
        next.channel = 1;
        next.is_24ghz = true;
        next.sco = 3;
        next.vht_width = PHY_WIDTH_40;
        next.rlc_command_version = RLC_CONFIG_VERSION;
        let mut actions = Vec::new();
        assert_eq!(
            update_phy_context(&mut current, next, true, 0, |cmd| {
                actions.push(if cmd.wire_id == rlc.wire_id {
                    "rlc"
                } else {
                    match u32::from_le_bytes(cmd.bytes[12..16].try_into().unwrap()) {
                        PHY_CONTEXT_ACTION_REMOVE => "remove",
                        PHY_CONTEXT_ACTION_ADD => "add",
                        _ => "modify",
                    }
                });
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(actions, ["remove", "add", "rlc"]);
        assert_eq!(current.channel, 1);
        assert!(current.is_24ghz);
        assert_eq!(current.sco, 3);
        assert_eq!(current.vht_width, PHY_WIDTH_40);
    }
}
