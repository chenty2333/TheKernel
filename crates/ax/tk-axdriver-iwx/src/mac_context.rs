//! Firmware MAC-context command builders from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CommandError, EncodedCommand, HostCommand};

pub const MAC_CONTEXT_COMMAND: u8 = 0x28;
pub const MAC_CONFIG_COMMAND: u8 = 0x08;
pub const MAC_CONF_GROUP: u8 = 0x03;
pub const ACTION_ADD: u32 = 1;
pub const ACTION_REMOVE: u32 = 3;
pub const MAC_TYPE_LISTENER: u32 = 2;
pub const MAC_TYPE_BSS_STA: u32 = 5;
pub const FILTER_ACCEPT_GROUP: u32 = 1 << 2;
pub const FILTER_PROMISC: u32 = 1 << 0;
pub const FILTER_CONTROL_AND_MGMT: u32 = 1 << 1;
pub const FILTER_BEACON: u32 = 1 << 6;
pub const FILTER_PROBE_REQUEST: u32 = 1 << 12;
pub const FILTER_CRC32: u32 = 1 << 11;
pub const MAC_QOS_UPDATE_EDCA: u32 = 1;
pub const MAC_QOS_TGN: u32 = 1 << 1;
pub const MAC_PROT_TGG: u32 = 1 << 3;
pub const MAC_PROT_HT: u32 = 1 << 23;
pub const MAC_PROT_FAT: u32 = 1 << 24;
pub const MAX_AC: usize = 4;
const LEGACY_COMMON_BYTES: usize = 100;
const LEGACY_STA_BYTES: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacContextError {
    InvalidAction,
    AlreadyActive,
    NotActive,
    UnsupportedMode,
    Command(CommandError),
}

impl From<CommandError> for MacContextError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EdcaParameters {
    pub ecw_min: u8,
    pub ecw_max: u8,
    pub aifsn: u8,
    pub txop_limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationMode {
    Monitor,
    Station,
}

#[derive(Debug, Clone, Copy)]
pub struct MacContextConfig {
    pub action: u32,
    pub id_and_color: u32,
    pub operation_mode: OperationMode,
    pub local_address: [u8; 6],
    pub bssid: [u8; 6],
    pub cck_rates: u32,
    pub ofdm_rates: u32,
    pub short_preamble: bool,
    pub short_slot: bool,
    pub edca: [EdcaParameters; MAX_AC],
    pub firmware_family_bz: bool,
    pub qos: bool,
    pub ht: bool,
    pub ht_protection: HtProtection,
    pub channel_sco: u8,
    pub use_protection: bool,
    pub associated: bool,
    pub assoc_id: u16,
    pub beacon_interval: u16,
    pub dtim_count: u8,
    pub dtim_period: u8,
    pub receive_timestamp: u32,
    pub beacon_timestamp: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HtProtection {
    None,
    Nonmember,
    NonHtMixed,
    TwentyMhz,
    Other,
}

/// Construct the common MAC-context wire fields and station timing payload.
// upstream: if_iwx.c iwx_mac_ctxt_cmd_common()
fn legacy_context_payload(config: &MacContextConfig) -> Result<Vec<u8>, MacContextError> {
    if config.action != ACTION_ADD && config.action != ACTION_REMOVE {
        return Err(MacContextError::InvalidAction);
    }
    let mut payload = alloc::vec![0; LEGACY_COMMON_BYTES + LEGACY_STA_BYTES];
    put_u32(&mut payload, 0, config.id_and_color);
    put_u32(&mut payload, 4, config.action);
    if config.action == ACTION_REMOVE {
        return Ok(payload);
    }
    let (mac_type, bssid) = match config.operation_mode {
        OperationMode::Monitor => (MAC_TYPE_LISTENER, [0xff; 6]),
        OperationMode::Station => (MAC_TYPE_BSS_STA, config.bssid),
    };
    put_u32(&mut payload, 8, mac_type);
    put_u32(&mut payload, 12, 0); // IWX_TSF_ID_A
    payload[16..22].copy_from_slice(&config.local_address);
    payload[24..30].copy_from_slice(&bssid);
    if config.operation_mode == OperationMode::Station {
        put_u32(&mut payload, 32, config.cck_rates);
        put_u32(&mut payload, 36, config.ofdm_rates);
        put_u32(
            &mut payload,
            44,
            if config.short_preamble { 1 << 5 } else { 0 },
        );
        put_u32(&mut payload, 48, if config.short_slot { 1 << 4 } else { 0 });
    }
    let mut filter = FILTER_ACCEPT_GROUP;
    if config.operation_mode == OperationMode::Monitor {
        filter |= FILTER_PROMISC
            | FILTER_CONTROL_AND_MGMT
            | FILTER_BEACON
            | FILTER_PROBE_REQUEST
            | FILTER_CRC32;
    } else if !config.associated || config.assoc_id == 0 || config.dtim_period == 0 {
        filter |= FILTER_BEACON;
    }
    put_u32(&mut payload, 52, filter);
    let mut qos_flags = 0;
    if config.operation_mode == OperationMode::Station {
        if config.qos {
            qos_flags |= MAC_QOS_UPDATE_EDCA;
        }
        if config.ht {
            qos_flags |= MAC_QOS_TGN;
        }
    }
    put_u32(&mut payload, 56, qos_flags);
    const GEN2_FIFO: [usize; MAX_AC] = [2, 1, 3, 4];
    const BZ_FIFO: [usize; MAX_AC] = [1, 0, 2, 3];
    if config.operation_mode == OperationMode::Station {
        for (ac, param) in config.edca.iter().enumerate() {
            let fifo = if config.firmware_family_bz {
                BZ_FIFO[ac]
            } else {
                GEN2_FIFO[ac]
            };
            let off = 60 + fifo * 8;
            put_u16(&mut payload, off, ecw(param.ecw_min));
            put_u16(&mut payload, off + 2, ecw(param.ecw_max));
            payload[off + 4] = param.aifsn;
            payload[off + 5] = 1 << fifo;
            put_u16(&mut payload, off + 6, param.txop_limit.wrapping_mul(32));
        }
    }
    if config.operation_mode == OperationMode::Station && config.ht {
        let protection = match config.ht_protection {
            HtProtection::None | HtProtection::Other => 0,
            HtProtection::Nonmember | HtProtection::NonHtMixed => MAC_PROT_HT | MAC_PROT_FAT,
            HtProtection::TwentyMhz if config.channel_sco == 1 || config.channel_sco == 3 => {
                MAC_PROT_HT | MAC_PROT_FAT
            }
            HtProtection::TwentyMhz => 0,
        };
        put_u32(&mut payload, 40, protection);
    }
    if config.operation_mode == OperationMode::Station && config.use_protection {
        let old = u32::from_le_bytes(payload[40..44].try_into().unwrap());
        put_u32(&mut payload, 40, old | MAC_PROT_TGG);
    }
    mac_context_fill_sta(config, &mut payload);
    Ok(payload)
}

/// Fill the version-2 station-specific MAC context fields.
// upstream: if_iwx.c iwx_mac_ctxt_cmd_fill_sta()
fn mac_context_fill_sta(config: &MacContextConfig, payload: &mut [u8]) {
    let sta = LEGACY_COMMON_BYTES;
    if config.operation_mode == OperationMode::Station {
        let dtim_offset = u32::from(config.dtim_count)
            .wrapping_mul(u32::from(config.beacon_interval))
            .wrapping_mul(1024);
        put_u32(payload, sta, u32::from(config.associated));
        if config.associated {
            put_u32(
                payload,
                sta + 4,
                config.receive_timestamp.wrapping_add(dtim_offset),
            );
            put_u64(
                payload,
                sta + 8,
                config.beacon_timestamp.wrapping_add(u64::from(dtim_offset)),
            );
        }
        put_u32(payload, sta + 16, u32::from(config.beacon_interval));
        put_u32(
            payload,
            sta + 24,
            u32::from(config.beacon_interval).wrapping_mul(u32::from(config.dtim_period)),
        );
        put_u32(payload, sta + 32, 10);
        put_u32(payload, sta + 36, u32::from(config.assoc_id & 0x3fff));
        if config.associated {
            put_u32(payload, sta + 40, config.receive_timestamp);
        }
    }
}

/// Build legacy MAC_CONTEXT_CMD, enforcing source active-state invariants.
// upstream: if_iwx.c iwx_mac_ctxt_cmd()
pub fn mac_context_command(
    config: &MacContextConfig,
    active: bool,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, MacContextError> {
    if config.action == ACTION_ADD && active {
        return Err(MacContextError::AlreadyActive);
    }
    if config.action == ACTION_REMOVE && !active {
        return Err(MacContextError::NotActive);
    }
    let payload = legacy_context_payload(config)?;
    let command = HostCommand {
        id: u32::from(MAC_CONTEXT_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

/// Build the MLD MAC_CONFIG command; the MLD API omits legacy QoS and TSF fields.
// upstream: if_iwx.c iwx_mld_mac_ctxt_cmd()
pub fn mld_mac_context_command(
    config: &MacContextConfig,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, MacContextError> {
    if config.action != ACTION_ADD && config.action != ACTION_REMOVE {
        return Err(MacContextError::InvalidAction);
    }
    let mut payload = alloc::vec![0; 52];
    put_u32(&mut payload, 0, config.id_and_color);
    put_u32(&mut payload, 4, config.action);
    if config.action != ACTION_REMOVE {
        put_u32(
            &mut payload,
            8,
            match config.operation_mode {
                OperationMode::Monitor => MAC_TYPE_LISTENER,
                OperationMode::Station => MAC_TYPE_BSS_STA,
            },
        );
        payload[12..18].copy_from_slice(&config.local_address);
        put_u32(&mut payload, 20, FILTER_ACCEPT_GROUP);
        if config.operation_mode == OperationMode::Monitor {
            put_u32(
                &mut payload,
                20,
                FILTER_ACCEPT_GROUP
                    | FILTER_PROMISC
                    | FILTER_CONTROL_AND_MGMT
                    | (1 << 3)
                    | FILTER_PROBE_REQUEST,
            );
        } else if !config.associated || config.assoc_id == 0 || config.dtim_period == 0 {
            put_u32(&mut payload, 20, FILTER_ACCEPT_GROUP | (1 << 3));
        }
        payload[36] = u8::from(config.associated);
        put_u16(&mut payload, 40, config.assoc_id & 0x3fff);
    }
    let command = HostCommand {
        id: (u32::from(MAC_CONF_GROUP) << 8) | u32::from(MAC_CONFIG_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

/// Enforce the legacy MAC context active bit after successful command delivery.
// upstream: if_iwx.c iwx_mac_ctxt_cmd()
pub fn update_mac_context<E>(
    active: &mut bool,
    action: u32,
    send: impl FnOnce(&EncodedCommand) -> Result<(), E>,
    command: &EncodedCommand,
) -> Result<(), MacContextUpdateError<E>> {
    match action {
        ACTION_ADD if *active => {
            return Err(MacContextUpdateError::Context(
                MacContextError::AlreadyActive,
            ));
        }
        ACTION_REMOVE if !*active => {
            return Err(MacContextUpdateError::Context(MacContextError::NotActive));
        }
        ACTION_ADD | ACTION_REMOVE => {}
        _ => {
            return Err(MacContextUpdateError::Context(
                MacContextError::InvalidAction,
            ));
        }
    }
    send(command).map_err(MacContextUpdateError::Send)?;
    *active = action == ACTION_ADD;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum MacContextUpdateError<E> {
    Context(MacContextError),
    Send(E),
}

fn ecw(value: u8) -> u16 {
    (1u16 << value).wrapping_sub(1)
}
fn put_u16(dst: &mut [u8], off: usize, value: u16) {
    dst[off..off + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(dst: &mut [u8], off: usize, value: u32) {
    dst[off..off + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(dst: &mut [u8], off: usize, value: u64) {
    dst[off..off + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(action: u32) -> MacContextConfig {
        MacContextConfig {
            action,
            id_and_color: 0x1234,
            operation_mode: OperationMode::Station,
            local_address: [0, 1, 2, 3, 4, 5],
            bssid: [6, 7, 8, 9, 10, 11],
            cck_rates: 3,
            ofdm_rates: 0x1f,
            short_preamble: true,
            short_slot: true,
            edca: [EdcaParameters::default(); MAX_AC],
            firmware_family_bz: false,
            qos: true,
            ht: true,
            ht_protection: HtProtection::NonHtMixed,
            channel_sco: 0,
            use_protection: false,
            associated: false,
            assoc_id: 0,
            beacon_interval: 100,
            dtim_count: 2,
            dtim_period: 3,
            receive_timestamp: 7,
            beacon_timestamp: 9,
        }
    }

    #[test]
    fn legacy_context_fields_match_packed_wire_layout() {
        let cmd = mac_context_command(&config(ACTION_ADD), false, 0, 0).unwrap();
        assert_eq!(cmd.bytes.len(), 8 + LEGACY_COMMON_BYTES + LEGACY_STA_BYTES);
        assert_eq!(&cmd.bytes[8..12], &0x1234u32.to_le_bytes());
        let p = &cmd.bytes[8..];
        assert_eq!(&p[8..12], &MAC_TYPE_BSS_STA.to_le_bytes());
        assert_eq!(&p[16..22], &[0, 1, 2, 3, 4, 5]);
        assert_eq!(&p[24..30], &[6, 7, 8, 9, 10, 11]);
        assert_eq!(
            u32::from_le_bytes(p[40..44].try_into().unwrap()),
            MAC_PROT_HT | MAC_PROT_FAT
        );
        assert_eq!(
            u32::from_le_bytes(p[52..56].try_into().unwrap()),
            FILTER_ACCEPT_GROUP | FILTER_BEACON
        );
    }

    #[test]
    fn monitor_mld_context_and_active_transition_preserve_source_policy() {
        let mut cfg = config(ACTION_ADD);
        cfg.operation_mode = OperationMode::Monitor;
        let mld = mld_mac_context_command(&cfg, 1, 0).unwrap();
        assert_eq!(mld.bytes.len(), 8 + 52);
        assert_eq!(&mld.bytes[8 + 12..8 + 18], &[0, 1, 2, 3, 4, 5]);
        let mut active = false;
        assert_eq!(
            update_mac_context(&mut active, ACTION_ADD, |_| Ok::<_, ()>(()), &mld),
            Ok(())
        );
        assert!(active);
        assert!(update_mac_context(&mut active, ACTION_ADD, |_| Ok::<_, ()>(()), &mld).is_err());
    }
}
