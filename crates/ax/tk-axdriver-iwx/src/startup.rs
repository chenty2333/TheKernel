//! Firmware init commands and final NIC bring-up order from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const BT_CONFIG_COMMAND: u8 = 0x9b;
pub const BT_COEX_WIFI: u32 = 3;
pub const SOC_CONFIGURATION_COMMAND: u8 = 0x01;
pub const SYSTEM_GROUP: u8 = 0x02;
pub const SOC_FLAG_DISCRETE: u32 = 1;
pub const SOC_FLAG_LOW_LATENCY: u32 = 1 << 1;
pub const SOC_LTR_DELAY_MASK: u32 = 0x0c;
pub const MCC_UPDATE_COMMAND: u8 = 0xc8;
pub const MCC_UPDATE_RESPONSE_MAX_VERSION: u8 = 8;
pub const MCC_SOURCE_OLD_FW: u8 = 0;
pub const MCC_SOURCE_GET_CURRENT: u8 = 0x10;
pub const PHY_OPS_GROUP: u8 = 0x04;
pub const TEMP_REPORTING_THRESHOLDS_COMMAND: u8 = 0x04;
pub const TEMP_REPORT_COMMAND_BYTES: usize = 20;
pub const MCC_COMMAND_BYTES: usize = 28;
pub const MCC_V4_HEADER_BYTES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupError {
    Command(CommandError),
    UnsupportedMccResponse(u8),
    InvalidMccResponse,
}
impl From<CommandError> for StartupError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Construct BT coexistence setup with Wi-Fi mode and no optional modules.
// upstream: if_iwx.c iwx_send_bt_init_conf()
pub fn bt_init_command(slot: u8) -> Result<EncodedCommand, StartupError> {
    let mut payload = [0u8; 8];
    payload[0..4].copy_from_slice(&BT_COEX_WIFI.to_le_bytes());
    let command = HostCommand {
        id: u32::from(BT_CONFIG_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocConfig {
    pub integrated: bool,
    pub ltr_delay: u32,
    pub scan_command_version: u8,
    pub low_latency_xtal: bool,
    pub xtal_latency: u32,
}

/// Build SOC_CONFIGURATION with v1 discrete or v2 independent flag semantics.
// upstream: if_iwx.c iwx_send_soc_conf()
pub fn soc_configuration_command(
    config: SocConfig,
    slot: u8,
) -> Result<EncodedCommand, StartupError> {
    let flags = if !config.integrated {
        SOC_FLAG_DISCRETE
    } else {
        (config.ltr_delay & SOC_LTR_DELAY_MASK)
            | if config.scan_command_version != 99
                && config.scan_command_version >= 2
                && config.low_latency_xtal
            {
                SOC_FLAG_LOW_LATENCY
            } else {
                0
            }
    };
    let mut payload = [0u8; 8];
    payload[0..4].copy_from_slice(&flags.to_le_bytes());
    payload[4..8].copy_from_slice(&config.xtal_latency.to_le_bytes());
    let command = HostCommand {
        id: (u32::from(SYSTEM_GROUP) << 8) | u32::from(SOC_CONFIGURATION_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Serialize a country-code request and source/API-selected MCC source ID.
// upstream: if_iwx.c iwx_send_update_mcc_cmd()
pub fn mcc_update_command(
    alpha2: [u8; 2],
    api_wifi_mcc_update: bool,
    lar_multi_mcc: bool,
    slot: u8,
) -> Result<EncodedCommand, StartupError> {
    let mut payload = [0u8; MCC_COMMAND_BYTES];
    payload[0..2].copy_from_slice(&u16::from_be_bytes(alpha2).to_le_bytes());
    payload[2] = if api_wifi_mcc_update || lar_multi_mcc {
        MCC_SOURCE_GET_CURRENT
    } else {
        MCC_SOURCE_OLD_FW
    };
    let command = HostCommand {
        id: u32::from(MCC_UPDATE_COMMAND),
        flags: CMD_WANT_RESPONSE,
        response_capacity: 4096,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MccUpdateResponse {
    pub status: u32,
    pub mcc: u16,
    pub cap: u16,
    pub source_id: u8,
    pub channels: Vec<u16>,
}

/// Validate MCC response API version and exact variable channel-array length.
// upstream: if_iwx.c iwx_send_update_mcc_cmd()
pub fn parse_mcc_update_response(
    response: &[u8],
    response_version: u8,
    mcc_11ax_support: bool,
) -> Result<MccUpdateResponse, StartupError> {
    if response_version >= MCC_UPDATE_RESPONSE_MAX_VERSION {
        return Err(StartupError::UnsupportedMccResponse(response_version));
    }
    let header = if mcc_11ax_support {
        MCC_V4_HEADER_BYTES
    } else {
        16
    };
    if response.len() < header {
        return Err(StartupError::InvalidMccResponse);
    }
    let (cap, source_id, count) = if mcc_11ax_support {
        (
            u16::from_le_bytes([response[6], response[7]]),
            response[12],
            u32::from_le_bytes(response[16..20].try_into().unwrap()),
        )
    } else {
        (
            u16::from(response[6]),
            response[7],
            u32::from_le_bytes(response[12..16].try_into().unwrap()),
        )
    };
    let count = usize::try_from(count).map_err(|_| StartupError::InvalidMccResponse)?;
    if header.checked_add(
        count
            .checked_mul(4)
            .ok_or(StartupError::InvalidMccResponse)?,
    ) != Some(response.len())
    {
        return Err(StartupError::InvalidMccResponse);
    }
    let mut channels = Vec::new();
    channels
        .try_reserve_exact(count)
        .map_err(|_| StartupError::InvalidMccResponse)?;
    for item in response[header..].chunks_exact(4) {
        channels.push(u16::from_le_bytes([item[0], item[1]]));
    }
    Ok(MccUpdateResponse {
        status: u32::from_le_bytes(response[0..4].try_into().unwrap()),
        mcc: u16::from_le_bytes(response[4..6].try_into().unwrap()),
        cap,
        source_id,
        channels,
    })
}

/// Build the empty temperature-threshold command that delegates kill/backoff to firmware.
// upstream: if_iwx.c iwx_send_temp_report_ths_cmd()
pub fn temperature_threshold_command(slot: u8) -> Result<EncodedCommand, StartupError> {
    let payload = [0u8; TEMP_REPORT_COMMAND_BYTES];
    let command = HostCommand {
        id: (u32::from(PHY_OPS_GROUP) << 8) | u32::from(TEMP_REPORTING_THRESHOLDS_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitHardwareConfig {
    pub send_phy_config: bool,
    pub dqa_supported: bool,
    pub temp_kill_supported: bool,
    pub lar_enabled: bool,
    pub init_pm_level: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitHardwareAction {
    RunInitMvm,
    AcquireNic,
    SendTxAntConfig,
    SendPhyConfig,
    SendBtConfig,
    SendSocConfig,
    SendDqa,
    InitPhyContexts,
    ConfigureLtr,
    SendTempThresholds,
    SetPower { level: u8, asynchronous: bool },
    UpdateMccDefault,
    ConfigureReducedScan,
    DisableBeaconFilter,
    UnlockNic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitHardwareOutcome<E> {
    Success,
    Busy,
    FailedAndUnlocked(InitHardwareAction, E),
    FailedWithNicLockHeld(InitHardwareAction, E),
}

/// Run final NIC setup in source order, including the distinct direct-return and unlock paths.
// upstream: if_iwx.c iwx_init_hw()
pub fn initialize_hardware<E>(
    config: InitHardwareConfig,
    mut execute: impl FnMut(InitHardwareAction) -> Result<(), E>,
) -> InitHardwareOutcome<E> {
    if let Err(error) = execute(InitHardwareAction::RunInitMvm) {
        return InitHardwareOutcome::FailedAndUnlocked(InitHardwareAction::RunInitMvm, error);
    }
    if execute(InitHardwareAction::AcquireNic).is_err() {
        return InitHardwareOutcome::Busy;
    }
    let mut early_actions = alloc::vec![InitHardwareAction::SendTxAntConfig];
    if config.send_phy_config {
        early_actions.push(InitHardwareAction::SendPhyConfig);
    }
    for action in early_actions {
        if let Err(error) = execute(action) {
            let _ = execute(InitHardwareAction::UnlockNic);
            return InitHardwareOutcome::FailedAndUnlocked(action, error);
        }
    }
    for action in [
        InitHardwareAction::SendBtConfig,
        InitHardwareAction::SendSocConfig,
    ] {
        if let Err(error) = execute(action) {
            return InitHardwareOutcome::FailedWithNicLockHeld(action, error);
        }
    }
    if config.dqa_supported {
        if let Err(error) = execute(InitHardwareAction::SendDqa) {
            return InitHardwareOutcome::FailedWithNicLockHeld(InitHardwareAction::SendDqa, error);
        }
    }
    for action in [
        InitHardwareAction::InitPhyContexts,
        InitHardwareAction::ConfigureLtr,
    ] {
        let _ = execute(action); // LTR failure is intentionally logged and ignored by source.
    }
    let power = InitHardwareAction::SetPower {
        level: config.init_pm_level,
        asynchronous: false,
    };
    let mut actions = alloc::vec![power];
    if config.temp_kill_supported {
        actions.insert(0, InitHardwareAction::SendTempThresholds);
    }
    if config.lar_enabled {
        actions.push(InitHardwareAction::UpdateMccDefault);
    }
    actions.extend([
        InitHardwareAction::ConfigureReducedScan,
        InitHardwareAction::DisableBeaconFilter,
    ]);
    for action in actions {
        if let Err(error) = execute(action) {
            let _ = execute(InitHardwareAction::UnlockNic);
            return InitHardwareOutcome::FailedAndUnlocked(action, error);
        }
    }
    let _ = config.init_pm_level;
    let _ = execute(InitHardwareAction::UnlockNic);
    InitHardwareOutcome::Success
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn bt_soc_mcc_and_temperature_commands_match_firmware_wire_values() {
        let bt = bt_init_command(0).unwrap();
        assert_eq!(&bt.bytes[8..16], &[3, 0, 0, 0, 0, 0, 0, 0]);
        let soc = soc_configuration_command(
            SocConfig {
                integrated: true,
                ltr_delay: 8,
                scan_command_version: 2,
                low_latency_xtal: true,
                xtal_latency: 0x1234,
            },
            0,
        )
        .unwrap();
        assert_eq!(&soc.bytes[8..12], &10u32.to_le_bytes());
        assert_eq!(&soc.bytes[12..16], &0x1234u32.to_le_bytes());
        let discrete = soc_configuration_command(
            SocConfig {
                integrated: false,
                ltr_delay: 0,
                scan_command_version: 99,
                low_latency_xtal: true,
                xtal_latency: 1,
            },
            0,
        )
        .unwrap();
        assert_eq!(&discrete.bytes[8..12], &SOC_FLAG_DISCRETE.to_le_bytes());
        let mcc = mcc_update_command(*b"ZZ", true, false, 0).unwrap();
        assert_eq!(&mcc.bytes[8..10], &u16::from_be_bytes(*b"ZZ").to_le_bytes());
        assert_eq!(mcc.bytes[10], MCC_SOURCE_GET_CURRENT);
        assert_eq!(
            temperature_threshold_command(0).unwrap().bytes.len(),
            8 + TEMP_REPORT_COMMAND_BYTES
        );
    }

    #[test]
    fn mcc_parser_checks_version_and_exact_channel_words() {
        let mut response = vec![0u8; 24];
        response[4..6].copy_from_slice(&0x555a_u16.to_le_bytes());
        response[6..8].copy_from_slice(&0x1234u16.to_le_bytes());
        response[12] = 0x10;
        response[16..20].copy_from_slice(&1u32.to_le_bytes());
        response[20..24].copy_from_slice(&0xabcdu32.to_le_bytes());
        let decoded = parse_mcc_update_response(&response, 4, true).unwrap();
        assert_eq!(decoded.mcc, 0x555a);
        assert_eq!(decoded.cap, 0x1234);
        assert_eq!(decoded.source_id, 0x10);
        assert_eq!(decoded.channels, [0xabcd]);
        assert_eq!(
            parse_mcc_update_response(&response, 8, true),
            Err(StartupError::UnsupportedMccResponse(8))
        );
        assert_eq!(
            parse_mcc_update_response(&response[..23], 4, true),
            Err(StartupError::InvalidMccResponse)
        );
    }

    #[test]
    fn init_hardware_preserves_source_lock_release_and_ignored_ltr_error_paths() {
        let config = InitHardwareConfig {
            send_phy_config: true,
            dqa_supported: true,
            temp_kill_supported: true,
            lar_enabled: true,
            init_pm_level: 0,
        };
        let mut actions = Vec::new();
        assert_eq!(
            initialize_hardware(config, |action| {
                actions.push(action);
                if action == InitHardwareAction::ConfigureLtr {
                    Err("ltr")
                } else {
                    Ok(())
                }
            }),
            InitHardwareOutcome::Success
        );
        assert_eq!(
            actions,
            [
                InitHardwareAction::RunInitMvm,
                InitHardwareAction::AcquireNic,
                InitHardwareAction::SendTxAntConfig,
                InitHardwareAction::SendPhyConfig,
                InitHardwareAction::SendBtConfig,
                InitHardwareAction::SendSocConfig,
                InitHardwareAction::SendDqa,
                InitHardwareAction::InitPhyContexts,
                InitHardwareAction::ConfigureLtr,
                InitHardwareAction::SendTempThresholds,
                InitHardwareAction::SetPower {
                    level: 0,
                    asynchronous: false
                },
                InitHardwareAction::UpdateMccDefault,
                InitHardwareAction::ConfigureReducedScan,
                InitHardwareAction::DisableBeaconFilter,
                InitHardwareAction::UnlockNic
            ]
        );
        assert_eq!(
            initialize_hardware(config, |action| {
                if action == InitHardwareAction::SendSocConfig {
                    Err("soc")
                } else {
                    Ok(())
                }
            }),
            InitHardwareOutcome::FailedWithNicLockHeld(InitHardwareAction::SendSocConfig, "soc")
        );
        let power = InitHardwareAction::SetPower {
            level: 0,
            asynchronous: false,
        };
        assert_eq!(
            initialize_hardware(config, |action| if action == power {
                Err("pm")
            } else {
                Ok(())
            }),
            InitHardwareOutcome::FailedAndUnlocked(power, "pm")
        );
    }
}
