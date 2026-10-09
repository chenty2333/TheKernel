//! U-APSD access-category and service-period policy from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

pub const WMM_AC_VO: u8 = 0x01;
pub const WMM_AC_VI: u8 = 0x02;
pub const WMM_AC_BK: u8 = 0x04;
pub const WMM_AC_BE: u8 = 0x08;
pub const WMM_AC_MASK: u8 = 0x0f;
pub const WMM_SP_ALL: u8 = 0;
pub const WMM_SP_2: u8 = 1;
pub const WMM_SP_4: u8 = 2;
pub const WMM_SP_6: u8 = 3;
pub const WMM_SP_MASK: u8 = 0x03;

/// Pick the highest-priority enabled non-ACM U-APSD trigger TID.
// upstream: if_iwx.c iwx_uapsd_qndp_tid()
pub fn uapsd_qndp_tid(requested_acs: u8, acm: [bool; 4]) -> u8 {
    if requested_acs & WMM_AC_VO != 0 && !acm[3] {
        6
    } else if requested_acs & WMM_AC_VI != 0 && !acm[2] {
        5
    } else if requested_acs & WMM_AC_BE != 0 && !acm[0] {
        0
    } else if requested_acs & WMM_AC_BK != 0 && !acm[1] {
        1
    } else {
        0
    }
}

/// Convert WMM QoS-info AC bits to firmware's duplicated trigger/delivery mask.
// upstream: if_iwx.c iwx_uapsd_acs()
pub const fn uapsd_ac_mask(requested_acs: u8) -> u8 {
    let mut acs = 0u8;
    if requested_acs & WMM_AC_BK != 0 {
        acs |= 1 << 0;
    }
    if requested_acs & WMM_AC_BE != 0 {
        acs |= 1 << 1;
    }
    if requested_acs & WMM_AC_VI != 0 {
        acs |= 1 << 2;
    }
    if requested_acs & WMM_AC_VO != 0 {
        acs |= 1 << 3;
    }
    acs | (acs << 4)
}

/// Convert WMM QoS-info AC bits to the MAC power-command U-APSD mask.
// upstream: if_iwx.c iwx_uapsd_ac_flags()
pub const fn uapsd_ac_flags(requested_acs: u8) -> u8 {
    let mut flags = 0u8;
    if requested_acs & WMM_AC_BE != 0 {
        flags |= 1 << 0;
    }
    if requested_acs & WMM_AC_BK != 0 {
        flags |= 1 << 1;
    }
    if requested_acs & WMM_AC_VI != 0 {
        flags |= 1 << 2;
    }
    if requested_acs & WMM_AC_VO != 0 {
        flags |= 1 << 3;
    }
    flags
}

/// Convert the WMM maximum service-period field to firmware's frame limit.
// upstream: if_iwx.c iwx_uapsd_sp_length()
pub const fn uapsd_service_period(max_service_period: u8) -> u8 {
    match max_service_period & WMM_SP_MASK {
        WMM_SP_2 => 2,
        WMM_SP_4 => 4,
        WMM_SP_6 => 6,
        _ => 128,
    }
}

/// Station power management follows OpenBSD's PMGTON level 3 by default.
pub const DEFAULT_STATION_POWER_LEVEL: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerConfig {
    pub monitor_mode: bool,
    pub dtim_skip: u8,
    pub level: u8,
    pub mac_active: bool,
    pub mac_id_color: u32,
    pub dtim_period: u16,
    pub beacon_interval_tu: u16,
    pub uapsd_node: bool,
    pub uapsd_supported: bool,
    pub uapsd_access_categories: u8,
    pub uapsd_acm: [bool; 4],
    pub uapsd_max_service_period: u8,
    pub asynchronous: bool,
}

/// Default-enabled station power-save policy; U-APSD remains negotiated per peer.
pub const fn default_station_power_config(
    mac_id_color: u32,
    beacon_interval_tu: u16,
) -> PowerConfig {
    PowerConfig {
        monitor_mode: false,
        dtim_skip: 0,
        level: DEFAULT_STATION_POWER_LEVEL,
        mac_active: true,
        mac_id_color,
        dtim_period: 1,
        beacon_interval_tu,
        uapsd_node: false,
        uapsd_supported: false,
        uapsd_access_categories: 0,
        uapsd_acm: [false; 4],
        uapsd_max_service_period: 0,
        asynchronous: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerError {
    InvalidLevel,
    Command(CommandError),
}

impl From<CommandError> for PowerError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerCommands {
    pub device: EncodedCommand,
    pub mac: Option<EncodedCommand>,
    pub beacon_abort_enabled: bool,
    pub keep_alive_seconds: u16,
}

const POWER_TIMEOUTS_MS: [[(u16, u16, i8); 6]; 3] = [
    [
        (0, 0, 0),
        (200, 500, 0),
        (200, 300, 0),
        (50, 100, 0),
        (50, 25, 1),
        (25, 25, 2),
    ],
    [
        (0, 0, 0),
        (200, 500, 0),
        (200, 300, 0),
        (50, 100, 0),
        (50, 25, 1),
        (25, 25, 2),
    ],
    [
        (0, 0, 0),
        (200, 500, 0),
        (200, 300, 0),
        (50, 100, 0),
        (50, 25, 0),
        (25, 25, 0),
    ],
];

/// Build device and MAC power-table commands using the source DTIM/level policy.
// upstream: if_iwx.c iwx_set_pslevel()
pub fn build_power_commands(
    config: PowerConfig,
    slot: u8,
    queue: u8,
) -> Result<Option<PowerCommands>, PowerError> {
    if config.monitor_mode {
        return Ok(None);
    }
    if config.level >= 6 {
        return Err(PowerError::InvalidLevel);
    }
    let dtim = if config.dtim_skip == 0 {
        1
    } else {
        config.dtim_skip
    };
    let range = if dtim <= 2 {
        0
    } else if dtim <= 10 {
        1
    } else {
        2
    };
    let (rx_timeout, tx_timeout, default_skip) = POWER_TIMEOUTS_MS[range][config.level as usize];
    let skip_dtim = if config.dtim_skip == 0 {
        0
    } else {
        default_skip
    };
    let is_async = if config.asynchronous { CMD_ASYNC } else { 0 };

    let mut device_payload = [0u8; 4];
    if config.level != 0 {
        device_payload[..2].copy_from_slice(&POWER_SAVE_ENABLE.to_le_bytes());
    }
    let device_command = HostCommand {
        id: POWER_TABLE_COMMAND,
        flags: is_async,
        response_capacity: 0,
        parts: &[&device_payload],
    };
    let device = EncodedCommand::encode(&device_command, slot, queue)?;

    if !config.mac_active {
        return Ok(Some(PowerCommands {
            device,
            mac: None,
            beacon_abort_enabled: false,
            keep_alive_seconds: 0,
        }));
    }

    let dtim_period = u64::from(config.dtim_period.max(1));
    let dtim_msec = dtim_period * u64::from(config.beacon_interval_tu);
    let keep_alive_seconds = (3 * dtim_msec)
        .max(POWER_KEEP_ALIVE_PERIOD_SEC * 1000)
        .div_ceil(1000)
        .min(u64::from(u16::MAX)) as u16;
    let mut mac_payload = [0u8; 40];
    mac_payload[0..4].copy_from_slice(&config.mac_id_color.to_le_bytes());
    let mut flags = 0u16;
    if config.level != 0 {
        flags |= POWER_SAVE_ENABLE | POWER_MANAGEMENT_ENABLE;
        mac_payload[8..12].copy_from_slice(&(u32::from(rx_timeout) * 1024).to_le_bytes());
        mac_payload[12..16].copy_from_slice(&(u32::from(tx_timeout) * 1024).to_le_bytes());
        if config.uapsd_node && config.uapsd_supported {
            flags |= POWER_ADVANCE_PM_ENABLE | POWER_UAPSD_MISBEHAVING_ENABLE;
            mac_payload[16..20].copy_from_slice(&UAPSD_RX_DATA_TIMEOUT.to_le_bytes());
            mac_payload[20..24].copy_from_slice(&UAPSD_TX_DATA_TIMEOUT.to_le_bytes());
            mac_payload[31] = uapsd_qndp_tid(config.uapsd_access_categories, config.uapsd_acm);
            mac_payload[32] = uapsd_ac_flags(config.uapsd_access_categories);
            mac_payload[33] = config.uapsd_max_service_period & WMM_SP_MASK;
        }
        if skip_dtim != 0 {
            flags |= POWER_SKIP_DTIM;
            mac_payload[25] = (skip_dtim + 1) as u8;
        }
    }
    mac_payload[4..6].copy_from_slice(&flags.to_le_bytes());
    mac_payload[6..8].copy_from_slice(&keep_alive_seconds.to_le_bytes());
    let mac_command = HostCommand {
        id: MAC_PM_POWER_TABLE_COMMAND,
        flags: is_async,
        response_capacity: 0,
        parts: &[&mac_payload],
    };
    let mac = EncodedCommand::encode(&mac_command, slot, queue)?;
    Ok(Some(PowerCommands {
        device,
        mac: Some(mac),
        beacon_abort_enabled: flags & POWER_MANAGEMENT_ENABLE != 0,
        keep_alive_seconds,
    }))
}

#[derive(Debug, PartialEq, Eq)]
pub enum PowerApplyError<E> {
    Send(E),
    BeaconAbort(E),
}

/// Send device-wide power first, then MAC power, then beacon-abort policy.
// upstream: if_iwx.c iwx_set_pslevel()
pub fn apply_power_commands<E>(
    commands: Option<&PowerCommands>,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
    mut set_beacon_abort: impl FnMut(bool) -> Result<(), E>,
) -> Result<(), PowerApplyError<E>> {
    let Some(commands) = commands else {
        return Ok(());
    };
    send(&commands.device).map_err(PowerApplyError::Send)?;
    if let Some(mac) = &commands.mac {
        send(mac).map_err(PowerApplyError::Send)?;
        set_beacon_abort(commands.beacon_abort_enabled).map_err(PowerApplyError::BeaconAbort)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uapsd_priority_masks_and_service_period_follow_source_bits() {
        let all = WMM_AC_VO | WMM_AC_VI | WMM_AC_BE | WMM_AC_BK;
        assert_eq!(uapsd_qndp_tid(all, [false; 4]), 6);
        assert_eq!(uapsd_qndp_tid(all, [false, false, false, true]), 5);
        assert_eq!(uapsd_qndp_tid(all, [false, false, true, true]), 0);
        assert_eq!(uapsd_qndp_tid(WMM_AC_BK, [false; 4]), 1);
        assert_eq!(uapsd_ac_mask(all), 0xff);
        assert_eq!(uapsd_ac_flags(WMM_AC_VO | WMM_AC_BE), 0b1001);
        assert_eq!(uapsd_service_period(WMM_SP_2), 2);
        assert_eq!(uapsd_service_period(WMM_SP_4), 4);
        assert_eq!(uapsd_service_period(WMM_SP_6), 6);
        assert_eq!(uapsd_service_period(WMM_SP_ALL), 128);
    }

    #[test]
    fn default_station_policy_enables_device_and_mac_power_save() {
        let config = default_station_power_config(0x1234, 100);
        assert_eq!(config.level, DEFAULT_STATION_POWER_LEVEL);
        assert!(config.mac_active && config.asynchronous);
        let commands = build_power_commands(config, 0, 0).unwrap().unwrap();
        let device = &commands.device.bytes[8..];
        assert_eq!(
            u16::from_le_bytes(device[..2].try_into().unwrap()),
            POWER_SAVE_ENABLE
        );
        let mac = &commands.mac.unwrap().bytes[8..];
        assert_eq!(
            u16::from_le_bytes(mac[4..6].try_into().unwrap()),
            POWER_SAVE_ENABLE | POWER_MANAGEMENT_ENABLE
        );
    }

    #[test]
    fn power_tables_preserve_dtim_ranges_and_command_order() {
        use alloc::vec::Vec;
        use core::cell::RefCell;

        let commands = build_power_commands(
            PowerConfig {
                monitor_mode: false,
                dtim_skip: 11,
                level: 4,
                mac_active: true,
                mac_id_color: 0x1234,
                dtim_period: 2,
                beacon_interval_tu: 100,
                uapsd_node: true,
                uapsd_supported: true,
                uapsd_access_categories: WMM_AC_VO | WMM_AC_BE,
                uapsd_acm: [false; 4],
                uapsd_max_service_period: WMM_SP_4,
                asynchronous: false,
            },
            2,
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(commands.keep_alive_seconds, 25);
        let mac = commands.mac.as_ref().unwrap();
        assert_eq!(mac.bytes.len(), 8 + 40);
        let payload = &mac.bytes[8..];
        assert_eq!(
            u16::from_le_bytes(payload[4..6].try_into().unwrap()),
            POWER_SAVE_ENABLE
                | POWER_MANAGEMENT_ENABLE
                | POWER_ADVANCE_PM_ENABLE
                | POWER_UAPSD_MISBEHAVING_ENABLE
        );
        assert_eq!(payload[25], 0); // DTIM>=11 level 4 disables the skip flag.
        assert_eq!(payload[31], 6);
        assert_eq!(payload[32], uapsd_ac_flags(WMM_AC_VO | WMM_AC_BE));
        assert_eq!(payload[33], WMM_SP_4);
        assert!(commands.beacon_abort_enabled);

        let events = RefCell::new(Vec::new());
        apply_power_commands(
            Some(&commands),
            |command| {
                events.borrow_mut().push(command.wire_id);
                Ok::<_, ()>(())
            },
            |enabled| {
                events.borrow_mut().push(if enabled { 1 } else { 0 });
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events.borrow()[0..2],
            [commands.device.wire_id, mac.wire_id]
        );
        assert_eq!(events.borrow()[2], 1);
    }

    #[test]
    fn beacon_filter_defaults_zero_disable_and_tracks_abort_state() {
        let enabled = beacon_filter_command(true, false, 1, 0).unwrap();
        assert_eq!(enabled.bytes.len(), 8 + BEACON_FILTER_CONFIG_BYTES);
        assert_eq!(&enabled.bytes[8..12], &5u32.to_le_bytes());
        assert_eq!(&enabled.bytes[32..36], &1u32.to_le_bytes());
        assert_eq!(&enabled.bytes[40..44], &50u32.to_le_bytes());
        assert_eq!(&enabled.bytes[48..52], &0u32.to_le_bytes());
        assert_eq!(&enabled.bytes[52..56], &0u32.to_le_bytes());
        assert_eq!(&enabled.bytes[56..60], &0u32.to_le_bytes());
        assert!(
            beacon_filter_command(false, false, 1, 0).unwrap().bytes[8..]
                .iter()
                .all(|byte| *byte == 0)
        );

        let mut state = BeaconFilterState::default();
        let mut sent = 0;
        update_beacon_abort(&mut state, true, 1, 0, |_| {
            sent += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent, 0);
        set_beacon_filter(&mut state, true, 1, 0, |_| {
            sent += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        update_beacon_abort(&mut state, true, 1, 0, |_| {
            sent += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(sent, 2);
        assert!(state.filter_enabled && state.beacon_abort_enabled);
    }
}
use crate::{CMD_ASYNC, CommandError, EncodedCommand, HostCommand};

pub const POWER_TABLE_COMMAND: u32 = 0x77;
pub const MAC_PM_POWER_TABLE_COMMAND: u32 = 0xa9;
pub const POWER_SAVE_ENABLE: u16 = 1 << 0;
pub const POWER_MANAGEMENT_ENABLE: u16 = 1 << 1;
pub const POWER_SKIP_DTIM: u16 = 1 << 2;
pub const POWER_ADVANCE_PM_ENABLE: u16 = 1 << 9;
pub const POWER_UAPSD_MISBEHAVING_ENABLE: u16 = 1 << 12;
pub const UAPSD_RX_DATA_TIMEOUT: u32 = 50 * 1000;
pub const UAPSD_TX_DATA_TIMEOUT: u32 = 50 * 1000;
pub const POWER_KEEP_ALIVE_PERIOD_SEC: u64 = 25;
pub const BEACON_FILTER_COMMAND: u32 = 0xd2;
pub const BEACON_FILTER_CONFIG_BYTES: usize = 56;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BeaconFilterState {
    pub filter_enabled: bool,
    pub beacon_abort_enabled: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BeaconFilterError<E> {
    Encode(CommandError),
    Send(E),
}

/// Build the default beacon-filter configuration or a zeroed disable command.
// upstream: if_iwx.c iwx_beacon_filter_send_cmd()
pub fn beacon_filter_command(
    enable_filter: bool,
    abort_enabled: bool,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; BEACON_FILTER_CONFIG_BYTES];
    if enable_filter {
        for (index, value) in [5u32, 1, 72, 112, 1, 5, 1, 0, 50, 6]
            .into_iter()
            .enumerate()
        {
            payload[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        payload[40..44].copy_from_slice(&u32::from(abort_enabled).to_le_bytes());
    }
    let command = HostCommand {
        id: BEACON_FILTER_COMMAND,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

/// Enable/disable filtering, recording state only after successful send.
pub fn set_beacon_filter<E>(
    state: &mut BeaconFilterState,
    enabled: bool,
    slot: u8,
    queue: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), BeaconFilterError<E>> {
    let command = beacon_filter_command(enabled, state.beacon_abort_enabled, slot, queue)
        .map_err(BeaconFilterError::Encode)?;
    send(&command).map_err(BeaconFilterError::Send)?;
    state.filter_enabled = enabled;
    Ok(())
}

/// Enable beacon filtering after RUN configuration.
// upstream: if_iwx.c iwx_enable_beacon_filter()
pub fn enable_beacon_filter<E>(
    state: &mut BeaconFilterState,
    slot: u8,
    queue: u8,
    send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), BeaconFilterError<E>> {
    set_beacon_filter(state, true, slot, queue, send)
}

/// Disable beacon filtering, preserving the prior state when command send fails.
// upstream: if_iwx.c iwx_disable_beacon_filter()
pub fn disable_beacon_filter<E>(
    state: &mut BeaconFilterState,
    slot: u8,
    queue: u8,
    send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), BeaconFilterError<E>> {
    set_beacon_filter(state, false, slot, queue, send)
}

/// Toggle beacon abort only when filtering is active; record the flag before
/// issuing the command, matching upstream's update order.
// upstream: if_iwx.c iwx_update_beacon_abort()
pub fn update_beacon_abort<E>(
    state: &mut BeaconFilterState,
    enabled: bool,
    slot: u8,
    queue: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), BeaconFilterError<E>> {
    if !state.filter_enabled {
        return Ok(());
    }
    state.beacon_abort_enabled = enabled;
    let command =
        beacon_filter_command(true, enabled, slot, queue).map_err(BeaconFilterError::Encode)?;
    send(&command).map_err(BeaconFilterError::Send)
}
