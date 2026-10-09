//! MAC-to-PHY binding context commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const BINDING_CONTEXT_COMMAND: u32 = 0x2b;
pub const MAX_MACS_IN_BINDING: usize = 3;
pub const INVALID_CONTEXT_ID: u32 = u32::MAX;
pub const LMAC_24_GHZ: u32 = 0;
pub const LMAC_5_GHZ: u32 = 1;
pub const CONTEXT_ACTION_ADD: u32 = 1;
pub const CONTEXT_ACTION_REMOVE: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingError {
    InvalidAction,
    AlreadyActive,
    NotActive,
    InvalidResponse,
    Command(CommandError),
}

impl From<CommandError> for BindingError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BindingState {
    pub active: bool,
}

/// Build the command unless MLD firmware replaces legacy binding contexts.
// upstream: if_iwx.c iwx_binding_cmd()
pub fn binding_command(
    use_mld_api: bool,
    action: u32,
    mac_id_color: u32,
    phy_id_color: u32,
    is_24ghz: bool,
    cdb_supported: bool,
    slot: u8,
    queue: u8,
) -> Result<Option<EncodedCommand>, BindingError> {
    if use_mld_api {
        return Ok(None);
    }
    if action != CONTEXT_ACTION_ADD && action != CONTEXT_ACTION_REMOVE {
        return Err(BindingError::InvalidAction);
    }
    let mut payload = [0u8; 28];
    payload[0..4].copy_from_slice(&mac_id_color.to_le_bytes());
    payload[4..8].copy_from_slice(&action.to_le_bytes());
    payload[8..12].copy_from_slice(&mac_id_color.to_le_bytes());
    for index in 1..MAX_MACS_IN_BINDING {
        let offset = 8 + index * 4;
        payload[offset..offset + 4].copy_from_slice(&INVALID_CONTEXT_ID.to_le_bytes());
    }
    payload[20..24].copy_from_slice(&phy_id_color.to_le_bytes());
    let lmac_id = if is_24ghz || !cdb_supported {
        LMAC_24_GHZ
    } else {
        LMAC_5_GHZ
    };
    payload[24..28].copy_from_slice(&lmac_id.to_le_bytes());
    let command = HostCommand {
        id: BINDING_CONTEXT_COMMAND,
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(Some(EncodedCommand::encode(&command, slot, queue)?))
}

/// Apply a binding add/remove transaction and update the source active flag.
// upstream: if_iwx.c iwx_binding_cmd()
pub fn update_binding<E>(
    state: &mut BindingState,
    command: Option<&EncodedCommand>,
    action: u32,
    mut send: impl FnMut(&EncodedCommand) -> Result<Vec<u8>, E>,
) -> Result<(), BindingUpdateError<E>> {
    if command.is_none() {
        return Ok(()); // MLD API intentionally bypasses legacy binding.
    }
    match action {
        CONTEXT_ACTION_ADD if state.active => {
            return Err(BindingUpdateError::Binding(BindingError::AlreadyActive));
        }
        CONTEXT_ACTION_REMOVE if !state.active => {
            return Err(BindingUpdateError::Binding(BindingError::NotActive));
        }
        CONTEXT_ACTION_ADD | CONTEXT_ACTION_REMOVE => {}
        _ => return Err(BindingUpdateError::Binding(BindingError::InvalidAction)),
    }
    let response = send(command.unwrap()).map_err(BindingUpdateError::Send)?;
    let status = response
        .get(..4)
        .ok_or(BindingUpdateError::Binding(BindingError::InvalidResponse))?;
    if u32::from_le_bytes(status.try_into().unwrap()) != 0 {
        return Err(BindingUpdateError::Binding(BindingError::InvalidResponse));
    }
    state.active = action == CONTEXT_ACTION_ADD;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum BindingUpdateError<E> {
    Binding(BindingError),
    Send(E),
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn binding_command_selects_lmac_and_mld_bypasses_legacy_payload() {
        assert!(
            binding_command(true, CONTEXT_ACTION_ADD, 0x1234, 0x5678, false, true, 0, 0)
                .unwrap()
                .is_none()
        );
        let command = binding_command(false, CONTEXT_ACTION_ADD, 0x1234, 0x5678, false, true, 3, 0)
            .unwrap()
            .unwrap();
        assert_eq!(command.flags, CMD_WANT_RESPONSE);
        assert_eq!(command.bytes.len(), 36);
        assert_eq!(&command.bytes[8..12], &0x1234u32.to_le_bytes());
        assert_eq!(&command.bytes[28..32], &0x5678u32.to_le_bytes());
        assert_eq!(&command.bytes[32..36], &LMAC_5_GHZ.to_le_bytes());
    }

    #[test]
    fn binding_active_state_changes_only_after_successful_firmware_status() {
        let command = binding_command(false, CONTEXT_ACTION_ADD, 1, 2, true, false, 0, 0)
            .unwrap()
            .unwrap();
        let mut state = BindingState::default();
        assert_eq!(
            update_binding(&mut state, Some(&command), CONTEXT_ACTION_ADD, |_| Ok::<
                _,
                (),
            >(
                vec![0; 4]
            )),
            Ok(())
        );
        assert!(state.active);
        assert_eq!(
            update_binding(&mut state, Some(&command), CONTEXT_ACTION_ADD, |_| Ok::<
                _,
                (),
            >(
                vec![0; 4]
            )),
            Err(BindingUpdateError::Binding(BindingError::AlreadyActive))
        );
    }
}
