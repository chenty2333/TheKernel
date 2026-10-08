//! Firmware initialization commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, EncodedCommand, HostCommand};

pub const TX_ANT_CONFIGURATION_CMD: u32 = 0x98;
pub const PHY_CONFIGURATION_CMD: u32 = 0x6a;
pub const DATA_PATH_GROUP: u8 = 0x05;
pub const DQA_ENABLE_CMD: u8 = 0x00;
pub const INIT_EXTENDED_CFG_CMD: u8 = 0x03;
pub const NVM_ACCESS_COMPLETE_CMD: u8 = 0x00;
pub const INIT_NVM: u32 = 1 << 1;
pub const SYSTEM_GROUP: u8 = 0x02;
pub const LTR_CONFIG_COMMAND: u32 = 0xee;
pub const LTR_CFG_FEATURE_ENABLE: u32 = 1;
pub const LTR_VALID_STATES: usize = 4;
pub const REGULATORY_AND_NVM_GROUP: u8 = 0x0c;

/// Serialize and frame the valid TX antenna mask.
// upstream: if_iwx.c iwx_send_tx_ant_cfg()
pub fn tx_antenna_command(
    valid_tx_ant: u8,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let payload = u32::from(valid_tx_ant).to_le_bytes();
    let command = HostCommand {
        id: TX_ANT_CONFIGURATION_CMD,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Serialize runtime PHY settings and calibration event/flow triggers.
// upstream: if_iwx.c iwx_send_phy_cfg_cmd()
pub fn phy_configuration_command(
    phy_config: u32,
    flow_trigger: u32,
    event_trigger: u32,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0; 12];
    payload[..4].copy_from_slice(&phy_config.to_le_bytes());
    payload[4..8].copy_from_slice(&flow_trigger.to_le_bytes());
    payload[8..12].copy_from_slice(&event_trigger.to_le_bytes());
    let command = HostCommand {
        id: PHY_CONFIGURATION_CMD,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Serialize the DQA command-queue selection and wide data-path command ID.
// upstream: if_iwx.c iwx_send_dqa_cmd()
pub fn dqa_enable_command(
    command_queue: u32,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, CommandError> {
    let payload = command_queue.to_le_bytes();
    let command = HostCommand {
        id: (u32::from(DATA_PATH_GROUP) << 8) | u32::from(DQA_ENABLE_CMD),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Tell init uCode that the host will issue NVM-access commands.
// upstream: if_iwx.c iwx_run_init_mvm_ucode()
pub fn init_extended_config_command(slot: u8, queue: u8) -> Result<EncodedCommand, CommandError> {
    let payload = INIT_NVM.to_le_bytes();
    let command = HostCommand {
        id: (u32::from(SYSTEM_GROUP) << 8) | u32::from(INIT_EXTENDED_CFG_CMD),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Notify init uCode that host NVM access has completed.
// upstream: if_iwx.c iwx_run_init_mvm_ucode()
pub fn nvm_access_complete_command(slot: u8, queue: u8) -> Result<EncodedCommand, CommandError> {
    let payload = [0u8; 4];
    let command = HostCommand {
        id: (u32::from(REGULATORY_AND_NVM_GROUP) << 8) | u32::from(NVM_ACCESS_COMPLETE_CMD),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, queue)
}

/// Configure LTR firmware support; the caller gates this on device capability.
// upstream: if_iwx.c iwx_config_ltr()
pub fn ltr_config_command(
    feature_enabled: bool,
    slot: u8,
    queue: u8,
) -> Result<Option<EncodedCommand>, CommandError> {
    if !feature_enabled {
        return Ok(None);
    }
    let mut payload = [0u8; (4 + LTR_VALID_STATES) * 4];
    payload[..4].copy_from_slice(&LTR_CFG_FEATURE_ENABLE.to_le_bytes());
    let command = HostCommand {
        id: LTR_CONFIG_COMMAND,
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(Some(EncodedCommand::encode(&command, slot, queue)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{command::LONG_GROUP, command_group_id};

    #[test]
    fn antenna_and_phy_commands_match_packed_source_fields() {
        let antenna = tx_antenna_command(0x05, 3, 0).unwrap();
        assert_eq!(
            antenna.wire_id,
            (u32::from(LONG_GROUP) << 8) | TX_ANT_CONFIGURATION_CMD
        );
        assert_eq!(&antenna.bytes[8..], &[5, 0, 0, 0]);

        let phy = phy_configuration_command(0x11223344, 0x55667788, 0x99aabbcc, 4, 0).unwrap();
        assert_eq!(
            phy.wire_id,
            (u32::from(LONG_GROUP) << 8) | PHY_CONFIGURATION_CMD
        );
        assert_eq!(
            &phy.bytes[8..],
            &[
                0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55, 0xcc, 0xbb, 0xaa, 0x99
            ]
        );
    }

    #[test]
    fn dqa_command_uses_data_path_group_and_little_endian_queue_id() {
        let command = dqa_enable_command(0x1234, 2, 0).unwrap();
        assert_eq!(command_group_id(command.wire_id), DATA_PATH_GROUP);
        assert_eq!(command.wire_id & 0xff, u32::from(DQA_ENABLE_CMD));
        assert_eq!(&command.bytes[8..], &[0x34, 0x12, 0, 0]);
    }

    #[test]
    fn init_mvm_nvm_gate_commands_match_group_and_payload() {
        let config = init_extended_config_command(1, 0).unwrap();
        assert_eq!(command_group_id(config.wire_id), SYSTEM_GROUP);
        assert_eq!(config.bytes[8..], INIT_NVM.to_le_bytes());
        let complete = nvm_access_complete_command(2, 0).unwrap();
        assert_eq!(command_group_id(complete.wire_id), REGULATORY_AND_NVM_GROUP);
        assert_eq!(complete.bytes[8..], [0; 4]);
    }

    #[test]
    fn ltr_configuration_is_capability_gated_and_zero_fills_reserved_values() {
        assert_eq!(ltr_config_command(false, 0, 0).unwrap(), None);
        let command = ltr_config_command(true, 2, 0).unwrap().unwrap();
        assert_eq!(command.bytes.len(), 8 + (4 + LTR_VALID_STATES) * 4);
        assert_eq!(&command.bytes[8..12], &LTR_CFG_FEATURE_ENABLE.to_le_bytes());
        assert!(command.bytes[12..].iter().all(|byte| *byte == 0));
    }
}
