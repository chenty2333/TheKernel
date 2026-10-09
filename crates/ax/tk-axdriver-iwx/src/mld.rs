//! MLD link/station context commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CommandError, EncodedCommand, HostCommand};

pub const MLD_MAC_CONF_GROUP: u8 = 3;
pub const LINK_CONFIG_COMMAND: u8 = 0x09;
pub const STA_CONFIG_COMMAND: u8 = 0x0a;
pub const STA_REMOVE_COMMAND: u8 = 0x0c;
pub const LINK_CONFIG_BYTES: usize = 208;
pub const STA_CONFIG_V1_BYTES: usize = 96;
pub const STA_CONFIG_V2_BYTES: usize = 104;
pub const INVALID_PHY_ID: u32 = u32::MAX;
pub const LINK_ACTION_ADD: u32 = 1;
pub const LINK_ACTION_MODIFY: u32 = 2;
pub const LINK_ACTION_REMOVE: u32 = 3;
pub const LINK_MODIFY_ACTIVE: u32 = 1 << 0;
pub const LINK_MODIFY_RATES_INFO: u32 = 1 << 1;
pub const LINK_MODIFY_PROTECTION_FLAGS: u32 = 1 << 2;
pub const LINK_MODIFY_QOS_PARAMS: u32 = 1 << 3;
pub const LINK_MODIFY_BEACON_TIMING: u32 = 1 << 4;
pub const LINK_PROTECT_TGG: u32 = 1 << 0;
pub const LINK_PROTECT_HT: u32 = 1 << 1;
pub const LINK_PROTECT_FAT: u32 = 1 << 2;
pub const MAC_QOS_UPDATE_EDCA: u32 = 1 << 0;
pub const MAC_QOS_TGN: u32 = 1 << 1;
pub const STA_MIMO: u32 = 1;
pub const STA_STATION_LINK: u32 = 0;
pub const STA_STATION_GENERAL: u32 = 1;
pub const MLD_REMOVE_STATION_ID: u32 = 0;
pub const FW_COMMAND_VERSION_UNKNOWN: u8 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MldHtProtection {
    None,
    NonMember,
    NonHtMixed,
    TwentyMhz,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MldEdcaAc {
    pub cw_min_exponent: u8,
    pub cw_max_exponent: u8,
    pub aifsn: u8,
    pub txop_limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MldLinkConfig {
    pub mac_id: u32,
    pub phy_id: u32,
    pub local_link_address: [u8; 6],
    pub active: bool,
    pub modify_mask: u32,
    pub cck_rates: u32,
    pub ofdm_rates: u32,
    pub short_preamble: bool,
    pub short_slot: bool,
    pub use_protection: bool,
    pub ht_protection: MldHtProtection,
    pub ht: bool,
    pub qos: bool,
    pub channel_is_40mhz: bool,
    pub secondary_channel_offset: u8,
    /// BE, BK, VI, VO in net80211 order.
    pub edca: [MldEdcaAc; 4],
    pub beacon_interval: u32,
    pub dtim_interval: u32,
    pub bz_family: bool,
}

/// Fill the packed MLD LINK_CONFIG_CMD with rates, EDCA and protection policy.
// upstream: if_iwx.c iwx_mld_modify_link_fill()
pub fn mld_modify_link_fill(config: MldLinkConfig) -> Result<Vec<u8>, CommandError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(LINK_CONFIG_BYTES)
        .map_err(|_| CommandError::PayloadTooLarge)?;
    bytes.resize(LINK_CONFIG_BYTES, 0);
    put_u32(&mut bytes, 0, LINK_ACTION_MODIFY);
    put_u32(&mut bytes, 4, 0); // link id
    put_u32(&mut bytes, 8, config.mac_id);
    put_u32(&mut bytes, 12, config.phy_id);
    bytes[16..22].copy_from_slice(&config.local_link_address);
    put_u32(&mut bytes, 24, config.modify_mask);
    put_u32(&mut bytes, 28, u32::from(config.active));
    put_u32(&mut bytes, 36, config.cck_rates);
    put_u32(&mut bytes, 40, config.ofdm_rates);
    put_u32(&mut bytes, 44, u32::from(config.short_preamble));
    put_u32(&mut bytes, 48, u32::from(config.short_slot));

    let mut protection = if config.use_protection {
        LINK_PROTECT_TGG
    } else {
        0
    };
    match config.ht_protection {
        MldHtProtection::NonMember | MldHtProtection::NonHtMixed => {
            protection |= LINK_PROTECT_HT | LINK_PROTECT_FAT;
        }
        MldHtProtection::TwentyMhz
            if config.channel_is_40mhz && config.secondary_channel_offset != 0 =>
        {
            protection |= LINK_PROTECT_HT | LINK_PROTECT_FAT;
        }
        MldHtProtection::None | MldHtProtection::TwentyMhz | MldHtProtection::Other => {}
    }
    put_u32(&mut bytes, 52, protection);
    let mut qos_flags = 0;
    if config.qos {
        qos_flags |= MAC_QOS_UPDATE_EDCA;
    }
    if config.ht {
        qos_flags |= MAC_QOS_TGN;
    }
    put_u32(&mut bytes, 56, qos_flags);

    let fifo = if config.bz_family {
        [1usize, 0, 2, 3]
    } else {
        [2usize, 1, 3, 4]
    };
    for (ac_index, ac) in config.edca.iter().enumerate() {
        let tx_fifo = fifo[ac_index];
        let offset = 60 + tx_fifo * 8;
        let cw_min = (1u32 << ac.cw_min_exponent).saturating_sub(1) as u16;
        let cw_max = (1u32 << ac.cw_max_exponent).saturating_sub(1) as u16;
        put_u16(&mut bytes, offset, cw_min);
        put_u16(&mut bytes, offset + 2, cw_max);
        bytes[offset + 4] = ac.aifsn;
        bytes[offset + 5] = 1 << tx_fifo;
        put_u16(&mut bytes, offset + 6, ac.txop_limit.wrapping_mul(32));
    }
    put_u32(&mut bytes, 136, config.beacon_interval);
    put_u32(&mut bytes, 140, config.dtim_interval);
    Ok(bytes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MldStationConfig {
    pub command_version: u8,
    pub update: bool,
    pub monitor_mode: bool,
    pub mac_id: u32,
    pub phy_id: Option<u32>,
    pub local_link_address: [u8; 6],
    pub peer_mld_address: [u8; 6],
    pub peer_link_address: [u8; 6],
    pub assoc_id: u32,
    pub use_mimo: bool,
    pub peer_vht: bool,
    pub peer_vht_rx_mcs_nss2: u8,
    pub peer_ht_rx_mcs_nss2: u8,
    pub ampdu_density: u8,
    pub ht_ampdu_exponent: u8,
    pub vht_ampdu_exponent: u8,
    pub mfp: bool,
    pub uapsd_supported: bool,
    pub uapsd_node: bool,
    pub uapsd_service_period: u8,
    pub uapsd_access_categories: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MldStationStep {
    AddLink,
    ModifyLink,
    ConfigureStation,
    RemoveStation,
    DeactivateLink,
    RemoveLink,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MldStationError<E> {
    UnsupportedCommandVersion(u8),
    Encode(CommandError),
    Send { step: MldStationStep, error: E },
}

/// Build the source v1/v2 STA_CONFIG_CMD payload for MLD-capable firmware.
// upstream: if_iwx.c iwx_mld_add_sta_cmd()
pub fn mld_station_config_command(
    config: MldStationConfig,
    slot: u8,
) -> Result<EncodedCommand, MldStationError<core::convert::Infallible>> {
    let bytes = mld_station_config_payload(config).map_err(|error| match error {
        MldStationError::UnsupportedCommandVersion(version) => {
            MldStationError::UnsupportedCommandVersion(version)
        }
        MldStationError::Encode(error) => MldStationError::Encode(error),
        MldStationError::Send { .. } => unreachable!(),
    })?;
    let command = HostCommand {
        id: (u32::from(MLD_MAC_CONF_GROUP) << 8) | u32::from(STA_CONFIG_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&bytes],
    };
    EncodedCommand::encode(&command, slot, 0).map_err(MldStationError::Encode)
}

fn mld_station_config_payload(
    config: MldStationConfig,
) -> Result<Vec<u8>, MldStationError<core::convert::Infallible>> {
    let size = match config.command_version {
        2 => STA_CONFIG_V2_BYTES,
        1 | FW_COMMAND_VERSION_UNKNOWN => STA_CONFIG_V1_BYTES,
        version => return Err(MldStationError::UnsupportedCommandVersion(version)),
    };
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(size)
        .map_err(|_| MldStationError::Encode(CommandError::PayloadTooLarge))?;
    payload.resize(size, 0);
    let station_id = if config.monitor_mode { 2 } else { 0 };
    let station_type = if config.monitor_mode {
        STA_STATION_GENERAL
    } else {
        STA_STATION_LINK
    };
    put_u32(&mut payload, 0, station_id);
    put_u32(&mut payload, 4, 0); // link id
    payload[8..14].copy_from_slice(&config.peer_mld_address);
    payload[16..22].copy_from_slice(&config.peer_link_address);
    put_u32(&mut payload, 24, station_type);
    put_u32(&mut payload, 28, config.assoc_id);
    put_u32(&mut payload, 36, u32::from(config.mfp));
    let mimo = config.use_mimo
        && if config.peer_vht {
            config.peer_vht_rx_mcs_nss2 != 3
        } else {
            config.peer_ht_rx_mcs_nss2 != 0
        };
    put_u32(&mut payload, 40, u32::from(mimo) * STA_MIMO);
    put_u32(&mut payload, 56, u32::from(config.ampdu_density));
    let aggregate_exponent = if config.peer_vht {
        config.vht_ampdu_exponent
    } else {
        config.ht_ampdu_exponent
    }
    .min(9);
    put_u32(&mut payload, 60, u32::from(aggregate_exponent));
    if config.uapsd_node && config.uapsd_supported {
        put_u32(&mut payload, 64, u32::from(config.uapsd_service_period));
        put_u32(&mut payload, 68, u32::from(config.uapsd_access_categories));
    }
    Ok(payload)
}

/// Send MLD link-add, link-modify, and station-config commands in source order.
// upstream: if_iwx.c iwx_mld_add_sta_cmd()
pub fn mld_add_station<E>(
    config: MldStationConfig,
    link: MldLinkConfig,
    slot: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), MldStationError<E>> {
    let _ = mld_station_command_size(config.command_version)
        .map_err(MldStationError::UnsupportedCommandVersion)?;
    if !config.update {
        let mut payload = zeroed_link_command().map_err(MldStationError::Encode)?;
        put_u32(&mut payload, 0, LINK_ACTION_ADD);
        put_u32(&mut payload, 4, 0);
        put_u32(&mut payload, 8, config.mac_id);
        put_u32(&mut payload, 12, config.phy_id.unwrap_or(INVALID_PHY_ID));
        payload[16..22].copy_from_slice(&config.local_link_address);
        let command = link_command(&payload, slot).map_err(MldStationError::Encode)?;
        send(&command).map_err(|error| MldStationError::Send {
            step: MldStationStep::AddLink,
            error,
        })?;
    }

    let mut link = link;
    link.mac_id = config.mac_id;
    link.phy_id = config.phy_id.unwrap_or(INVALID_PHY_ID);
    link.local_link_address = config.local_link_address;
    link.active = true;
    link.modify_mask = LINK_MODIFY_ACTIVE | LINK_MODIFY_RATES_INFO;
    if config.update {
        link.modify_mask |=
            LINK_MODIFY_PROTECTION_FLAGS | LINK_MODIFY_QOS_PARAMS | LINK_MODIFY_BEACON_TIMING;
    }
    let link_payload = mld_modify_link_fill(link).map_err(MldStationError::Encode)?;
    let link = link_command(&link_payload, slot).map_err(MldStationError::Encode)?;
    send(&link).map_err(|error| MldStationError::Send {
        step: MldStationStep::ModifyLink,
        error,
    })?;

    let station = mld_station_config_command(config, slot).map_err(|error| match error {
        MldStationError::UnsupportedCommandVersion(version) => {
            MldStationError::UnsupportedCommandVersion(version)
        }
        MldStationError::Encode(error) => MldStationError::Encode(error),
        MldStationError::Send { .. } => unreachable!(),
    })?;
    send(&station).map_err(|error| MldStationError::Send {
        step: MldStationStep::ConfigureStation,
        error,
    })
}

/// Remove the station, deactivate its link, and then remove the link context.
// upstream: if_iwx.c iwx_mld_rm_sta_cmd()
pub fn mld_remove_station<E>(
    link: MldLinkConfig,
    slot: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), MldStationError<E>> {
    let remove_station = [MLD_REMOVE_STATION_ID.to_le_bytes(), 0u32.to_le_bytes()].concat();
    let command = command(
        MLD_MAC_CONF_GROUP,
        STA_REMOVE_COMMAND,
        &remove_station,
        slot,
    )
    .map_err(MldStationError::Encode)?;
    send(&command).map_err(|error| MldStationError::Send {
        step: MldStationStep::RemoveStation,
        error,
    })?;

    let mut inactive = link;
    inactive.active = false;
    inactive.modify_mask = LINK_MODIFY_ACTIVE;
    let payload = mld_modify_link_fill(inactive).map_err(MldStationError::Encode)?;
    let command = link_command(&payload, slot).map_err(MldStationError::Encode)?;
    send(&command).map_err(|error| MldStationError::Send {
        step: MldStationStep::DeactivateLink,
        error,
    })?;

    let mut payload = zeroed_link_command().map_err(MldStationError::Encode)?;
    put_u32(&mut payload, 0, LINK_ACTION_REMOVE);
    put_u32(&mut payload, 4, 0);
    payload[166] = 0;
    let command = link_command(&payload, slot).map_err(MldStationError::Encode)?;
    send(&command).map_err(|error| MldStationError::Send {
        step: MldStationStep::RemoveLink,
        error,
    })
}

fn mld_station_command_size(version: u8) -> Result<usize, u8> {
    match version {
        2 => Ok(STA_CONFIG_V2_BYTES),
        1 | FW_COMMAND_VERSION_UNKNOWN => Ok(STA_CONFIG_V1_BYTES),
        other => Err(other),
    }
}

fn zeroed_link_command() -> Result<Vec<u8>, CommandError> {
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(LINK_CONFIG_BYTES)
        .map_err(|_| CommandError::PayloadTooLarge)?;
    payload.resize(LINK_CONFIG_BYTES, 0);
    Ok(payload)
}

fn link_command(payload: &[u8], slot: u8) -> Result<EncodedCommand, CommandError> {
    command(MLD_MAC_CONF_GROUP, LINK_CONFIG_COMMAND, payload, slot)
}

fn command(
    group: u8,
    opcode: u8,
    payload: &[u8],
    slot: u8,
) -> Result<EncodedCommand, CommandError> {
    let command = HostCommand {
        id: (u32::from(group) << 8) | u32::from(opcode),
        flags: 0,
        response_capacity: 0,
        parts: &[payload],
    };
    EncodedCommand::encode(&command, slot, 0)
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn link_config() -> MldLinkConfig {
        MldLinkConfig {
            mac_id: 1,
            phy_id: 2,
            local_link_address: [0, 1, 2, 3, 4, 5],
            active: true,
            modify_mask: LINK_MODIFY_ACTIVE | LINK_MODIFY_RATES_INFO,
            cck_rates: 0x3f,
            ofdm_rates: 0x1fe0,
            short_preamble: true,
            short_slot: true,
            use_protection: true,
            ht_protection: MldHtProtection::NonHtMixed,
            ht: true,
            qos: true,
            channel_is_40mhz: true,
            secondary_channel_offset: 1,
            edca: [MldEdcaAc {
                cw_min_exponent: 4,
                cw_max_exponent: 10,
                aifsn: 3,
                txop_limit: 47,
            }; 4],
            beacon_interval: 100,
            dtim_interval: 200,
            bz_family: false,
        }
    }

    fn station_config(update: bool) -> MldStationConfig {
        MldStationConfig {
            command_version: 2,
            update,
            monitor_mode: false,
            mac_id: 1,
            phy_id: Some(2),
            local_link_address: [0, 1, 2, 3, 4, 5],
            peer_mld_address: [6, 7, 8, 9, 10, 11],
            peer_link_address: [6, 7, 8, 9, 10, 11],
            assoc_id: 7,
            use_mimo: true,
            peer_vht: true,
            peer_vht_rx_mcs_nss2: 2,
            peer_ht_rx_mcs_nss2: 1,
            ampdu_density: 6,
            ht_ampdu_exponent: 3,
            vht_ampdu_exponent: 9,
            mfp: true,
            uapsd_supported: true,
            uapsd_node: true,
            uapsd_service_period: 2,
            uapsd_access_categories: 5,
        }
    }

    #[test]
    fn link_command_maps_edca_fifo_protection_and_timing_fields() {
        let bytes = mld_modify_link_fill(link_config()).unwrap();
        assert_eq!(bytes.len(), LINK_CONFIG_BYTES);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            LINK_ACTION_MODIFY
        );
        assert_eq!(&bytes[16..22], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(
            u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
            LINK_PROTECT_TGG | LINK_PROTECT_HT | LINK_PROTECT_FAT
        );
        assert_eq!(
            u32::from_le_bytes(bytes[56..60].try_into().unwrap()),
            MAC_QOS_UPDATE_EDCA | MAC_QOS_TGN
        );
        // BE FIFO2 in Gen2, cwmin=(2^4)-1, txop is expressed in 32-us units.
        assert_eq!(
            u16::from_le_bytes(bytes[60 + 2 * 8..62 + 2 * 8].try_into().unwrap()),
            15
        );
        assert_eq!(
            u16::from_le_bytes(bytes[60 + 2 * 8 + 6..60 + 2 * 8 + 8].try_into().unwrap()),
            1504
        );
        assert_eq!(u32::from_le_bytes(bytes[136..140].try_into().unwrap()), 100);
        assert_eq!(u32::from_le_bytes(bytes[140..144].try_into().unwrap()), 200);
    }

    #[test]
    fn mld_station_command_versions_monitor_ids_and_uapsd_fields_match() {
        let command = mld_station_config_command(station_config(false), 4).unwrap();
        assert_eq!(
            command.wire_id,
            (u32::from(MLD_MAC_CONF_GROUP) << 8) | u32::from(STA_CONFIG_COMMAND)
        );
        assert_eq!(command.bytes.len(), 8 + STA_CONFIG_V2_BYTES);
        assert_eq!(
            u32::from_le_bytes(command.bytes[8..12].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_le_bytes(command.bytes[8 + 40..8 + 44].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_le_bytes(command.bytes[8 + 60..8 + 64].try_into().unwrap()),
            9
        );
        let mut monitor = station_config(true);
        monitor.monitor_mode = true;
        let command = mld_station_config_command(monitor, 1).unwrap();
        assert_eq!(command.bytes.len(), 8 + STA_CONFIG_V2_BYTES);
        assert_eq!(
            u32::from_le_bytes(command.bytes[8..12].try_into().unwrap()),
            2
        );
        assert_eq!(&command.bytes[8 + 8..8 + 14], &[6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn mld_add_remove_commands_preserve_source_order_and_short_circuit() {
        let mut sent = Vec::new();
        mld_add_station(station_config(false), link_config(), 0, |command| {
            sent.push(command.wire_id);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0] & 0xff, u32::from(LINK_CONFIG_COMMAND));
        assert_eq!(sent[1] & 0xff, u32::from(LINK_CONFIG_COMMAND));
        assert_eq!(sent[2] & 0xff, u32::from(STA_CONFIG_COMMAND));

        sent.clear();
        mld_remove_station(link_config(), 0, |command| {
            sent.push(command.wire_id);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0] & 0xff, u32::from(STA_REMOVE_COMMAND));
        assert_eq!(sent[1] & 0xff, u32::from(LINK_CONFIG_COMMAND));
        assert_eq!(sent[2] & 0xff, u32::from(LINK_CONFIG_COMMAND));
    }
}
