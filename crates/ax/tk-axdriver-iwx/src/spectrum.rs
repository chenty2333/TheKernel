//! Smart-FIFO command construction from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CMD_ASYNC, CommandError, EncodedCommand, HostCommand};

pub const SF_CONFIG_COMMAND: u8 = 0xd1;
pub const SF_LONG_DELAY_ON: u32 = 0;
pub const SF_FULL_ON: u32 = 1;
pub const SF_UNINIT: u32 = 2;
pub const SF_INIT_OFF: u32 = 3;
pub const SF_SCENARIO_COUNT: usize = 5;
pub const SF_TIMEOUT_TYPE_COUNT: usize = 2;
pub const SF_WATERMARK_SCAN: u32 = 4096;
pub const SF_WATERMARK_SISO: u32 = 4096;
pub const SF_WATERMARK_MIMO2: u32 = 8192;
pub const SF_WATERMARK_LEGACY: u32 = 4096;
pub const SF_LONG_DELAY_AGING: u32 = 1_000_000;
pub const SF_CONFIG_BYTES: usize = 212;

const FULL_DEFAULT: [[u32; 2]; 5] = [[400, 160], [400, 160], [400, 160], [400, 160], [400, 160]];
const FULL_ASSOC: [[u32; 2]; 5] = [
    [2016, 320],
    [2016, 320],
    [10_016, 2016],
    [2016, 320],
    [2016, 320],
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpectrumError {
    InvalidState(u32),
    Command(CommandError),
}
impl From<CommandError> for SpectrumError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Fill state, watermarks, 1-second long-delay timers and association timeouts.
// upstream: if_iwx.c iwx_fill_sf_command()
pub fn fill_spectrum_payload(
    state: u32,
    associated: bool,
    node_ht: bool,
    second_rx_mcs: u8,
) -> Result<[u8; SF_CONFIG_BYTES], SpectrumError> {
    if !matches!(state, SF_UNINIT | SF_INIT_OFF | SF_FULL_ON) {
        return Err(SpectrumError::InvalidState(state));
    }
    let mut payload = [0u8; SF_CONFIG_BYTES];
    payload[0..4].copy_from_slice(&state.to_le_bytes());
    payload[4..8].copy_from_slice(&SF_WATERMARK_SCAN.to_le_bytes());
    let watermark = if !associated {
        SF_WATERMARK_MIMO2
    } else if node_ht && second_rx_mcs != 0 {
        SF_WATERMARK_MIMO2
    } else if node_ht {
        SF_WATERMARK_SISO
    } else {
        SF_WATERMARK_LEGACY
    };
    payload[8..12].copy_from_slice(&watermark.to_le_bytes());
    for index in 0..SF_SCENARIO_COUNT * SF_TIMEOUT_TYPE_COUNT {
        let offset = 12 + index * 4;
        payload[offset..offset + 4].copy_from_slice(&SF_LONG_DELAY_AGING.to_le_bytes());
    }
    let full = if associated { FULL_ASSOC } else { FULL_DEFAULT };
    for scenario in 0..SF_SCENARIO_COUNT {
        for timeout in 0..SF_TIMEOUT_TYPE_COUNT {
            let offset = 52 + (scenario * SF_TIMEOUT_TYPE_COUNT + timeout) * 4;
            payload[offset..offset + 4].copy_from_slice(&full[scenario][timeout].to_le_bytes());
        }
    }
    Ok(payload)
}

/// Build asynchronous SF_CFG unless firmware API advertises smart-FIFO offload.
// upstream: if_iwx.c iwx_sf_config()
pub fn spectrum_config_command(
    new_state: u32,
    associated: bool,
    node_ht: bool,
    second_rx_mcs: u8,
    smart_fifo_offload: bool,
    slot: u8,
) -> Result<Option<EncodedCommand>, SpectrumError> {
    if smart_fifo_offload {
        return Ok(None);
    }
    let payload = fill_spectrum_payload(new_state, associated, node_ht, second_rx_mcs)?;
    let command = HostCommand {
        id: u32::from(SF_CONFIG_COMMAND),
        flags: CMD_ASYNC,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(Some(EncodedCommand::encode(&command, slot, 0)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_fifo_payload_uses_profile_specific_watermarks_and_timeout_tables() {
        let payload = fill_spectrum_payload(SF_FULL_ON, true, true, 1).unwrap();
        assert_eq!(payload.len(), SF_CONFIG_BYTES);
        assert_eq!(&payload[4..12], &[0, 16, 0, 0, 0, 32, 0, 0]);
        assert_eq!(
            u32::from_le_bytes(payload[12..16].try_into().unwrap()),
            SF_LONG_DELAY_AGING
        );
        assert_eq!(
            u32::from_le_bytes(payload[52..56].try_into().unwrap()),
            2016
        );
        assert_eq!(
            u32::from_le_bytes(payload[68..72].try_into().unwrap()),
            10_016
        );
        let fallback = fill_spectrum_payload(SF_INIT_OFF, false, false, 0).unwrap();
        assert_eq!(
            u32::from_le_bytes(fallback[8..12].try_into().unwrap()),
            SF_WATERMARK_MIMO2
        );
        assert_eq!(
            u32::from_le_bytes(fallback[52..56].try_into().unwrap()),
            400
        );
    }

    #[test]
    fn smart_fifo_offload_skips_config_and_supported_states_send_async() {
        assert_eq!(
            spectrum_config_command(SF_FULL_ON, true, true, 1, true, 0).unwrap(),
            None
        );
        let command = spectrum_config_command(SF_UNINIT, false, false, 0, false, 3)
            .unwrap()
            .unwrap();
        assert_eq!(command.wire_id, (1 << 8) | u32::from(SF_CONFIG_COMMAND));
        assert_eq!(command.flags, CMD_ASYNC);
        assert_eq!(command.bytes.len(), 8 + SF_CONFIG_BYTES);
        assert_eq!(
            spectrum_config_command(7, false, false, 0, false, 0),
            Err(SpectrumError::InvalidState(7))
        );
    }
}
