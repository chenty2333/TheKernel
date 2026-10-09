//! Association session-protection commands from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, EncodedCommand, HostCommand};

pub const MAC_CONF_GROUP: u8 = 3;
pub const SESSION_PROTECTION_COMMAND: u8 = 0x05;
pub const SESSION_PROTECT_ASSOC: u32 = 0;
pub const SESSION_PROTECT_ACTION_ADD: u32 = 1;
pub const SESSION_PROTECT_ACTION_REMOVE: u32 = 3;
pub const SESSION_PROTECTION_PAYLOAD_BYTES: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionProtectionState {
    pub active: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SessionProtectionError<E> {
    Encode(CommandError),
    Send(E),
}

/// Serialize an association session-protection request with its fixed fields.
fn session_protection_command(
    id_and_color: u32,
    action: u32,
    duration_tu: u32,
    slot: u8,
) -> Result<EncodedCommand, CommandError> {
    let mut payload = [0u8; SESSION_PROTECTION_PAYLOAD_BYTES];
    payload[0..4].copy_from_slice(&id_and_color.to_le_bytes());
    payload[4..8].copy_from_slice(&action.to_le_bytes());
    payload[8..12].copy_from_slice(&SESSION_PROTECT_ASSOC.to_le_bytes());
    payload[12..16].copy_from_slice(&duration_tu.to_le_bytes());
    let command = HostCommand {
        id: (u32::from(MAC_CONF_GROUP) << 8) | u32::from(SESSION_PROTECTION_COMMAND),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Schedule association protection and publish active state only on send success.
// upstream: if_iwx.c iwx_schedule_session_protection()
pub fn schedule_session_protection<E>(
    state: &mut SessionProtectionState,
    id_and_color: u32,
    duration_tu: u32,
    slot: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<(), SessionProtectionError<E>> {
    let command =
        session_protection_command(id_and_color, SESSION_PROTECT_ACTION_ADD, duration_tu, slot)
            .map_err(SessionProtectionError::Encode)?;
    send(&command).map_err(SessionProtectionError::Send)?;
    state.active = true;
    Ok(())
}

/// Remove association protection only while its session event remains active.
// upstream: if_iwx.c iwx_unprotect_session()
pub fn unprotect_session<E>(
    state: &mut SessionProtectionState,
    id_and_color: u32,
    slot: u8,
    mut send: impl FnMut(&EncodedCommand) -> Result<(), E>,
) -> Result<bool, SessionProtectionError<E>> {
    if !state.active {
        return Ok(false);
    }
    let command = session_protection_command(id_and_color, SESSION_PROTECT_ACTION_REMOVE, 0, slot)
        .map_err(SessionProtectionError::Encode)?;
    send(&command).map_err(SessionProtectionError::Send)?;
    state.active = false;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn session_protection_add_remove_and_active_flag_follow_send_result() {
        let mut state = SessionProtectionState::default();
        let mut sent = Vec::new();
        schedule_session_protection(&mut state, 0x1122_3344, 500, 1, |command| {
            sent.push(command.bytes.clone());
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(state.active);
        assert_eq!(sent[0][0..2], [SESSION_PROTECTION_COMMAND, MAC_CONF_GROUP]);
        assert_eq!(sent[0][8..12], 0x1122_3344u32.to_le_bytes());
        assert_eq!(sent[0][12..16], SESSION_PROTECT_ACTION_ADD.to_le_bytes());
        assert_eq!(sent[0][20..24], 500u32.to_le_bytes());
        assert_eq!(&sent[0][24..], &[0; 8]);

        assert!(
            unprotect_session(&mut state, 0x1122_3344, 2, |command| {
                sent.push(command.bytes.clone());
                Ok::<_, ()>(())
            })
            .unwrap()
        );
        assert!(!state.active);
        assert_eq!(sent[1][12..16], SESSION_PROTECT_ACTION_REMOVE.to_le_bytes());
        assert_eq!(sent[1][20..24], [0; 4]);
        assert!(!unprotect_session(&mut state, 0, 0, |_| Ok::<_, ()>(())).unwrap());
    }

    #[test]
    fn failed_schedule_does_not_publish_active_session_state() {
        let mut state = SessionProtectionState::default();
        assert_eq!(
            schedule_session_protection(&mut state, 7, 30, 0, |_| Err("send")),
            Err(SessionProtectionError::Send("send"))
        );
        assert!(!state.active);
    }
}
