//! Statistics command handling translated from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CMD_ASYNC, CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const STATISTICS_COMMAND: u8 = 0x9c;
pub const SYSTEM_GROUP: u8 = 0x02;
pub const SYSTEM_STATISTICS_COMMAND: u8 = 0x0f;
pub const STATISTICS_CLEAR: u32 = 1;
pub const STATS_CONFIG_ON_DEMAND: u32 = 1 << 1;
pub const STATS_TYPE_OPERATIONAL: u32 = 1;
pub const STATS_TYPE_OPERATIONAL_PART1: u32 = 1 << 1;
pub const SYSTEM_STATISTICS_END_NOTIFICATION: u8 = 0xfd;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsError {
    Command(CommandError),
}
impl From<CommandError> for StatisticsError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

/// Build the legacy statistics clear command, retaining its response slot.
// upstream: if_iwx.c iwx_send_statistics_cmd_clear()
pub fn legacy_statistics_clear_command(
    response_capacity: usize,
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, StatisticsError> {
    let payload = STATISTICS_CLEAR.to_le_bytes();
    let command = HostCommand {
        id: u32::from(STATISTICS_COMMAND),
        flags: CMD_WANT_RESPONSE,
        response_capacity,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

/// Build the asynchronous system-statistics request; completion arrives as a notification.
// upstream: if_iwx.c iwx_send_system_statistics_cmd_clear()
pub fn system_statistics_clear_command(
    slot: u8,
    queue: u8,
) -> Result<EncodedCommand, StatisticsError> {
    let mut payload = [0u8; 12];
    payload[0..4].copy_from_slice(&STATS_CONFIG_ON_DEMAND.to_le_bytes());
    payload[8..12]
        .copy_from_slice(&(STATS_TYPE_OPERATIONAL | STATS_TYPE_OPERATIONAL_PART1).to_le_bytes());
    let command = HostCommand {
        id: (u32::from(SYSTEM_GROUP) << 8) | u32::from(SYSTEM_STATISTICS_COMMAND),
        flags: CMD_ASYNC,
        response_capacity: 0,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, queue)?)
}

/// Select the source-versioned statistics command; unknown command versions use legacy.
// upstream: if_iwx.c iwx_clear_statistics()
pub fn statistics_clear_command(
    version: u32,
    response_capacity: usize,
    slot: u8,
    queue: u8,
) -> Result<Option<EncodedCommand>, StatisticsError> {
    match version {
        99 => Ok(Some(legacy_statistics_clear_command(
            response_capacity,
            slot,
            queue,
        )?)),
        1 => Ok(Some(system_statistics_clear_command(slot, queue)?)),
        _ => Ok(None),
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SystemStatisticsWait {
    pub cleared: bool,
}

/// Reset the notification latch before the asynchronous command is submitted.
// upstream: if_iwx.c iwx_send_system_statistics_cmd_clear()
pub fn begin_system_statistics_clear(wait: &mut SystemStatisticsWait) {
    wait.cleared = false;
}

/// Record SYSTEM_STATISTICS_END_NOTIF, waking the waiting clear operation.
// upstream: if_iwx.c iwx_rx_pkt()
pub fn system_statistics_end_notification(wait: &mut SystemStatisticsWait) {
    wait.cleared = true;
}

/// Wait in one-second source intervals until notification or interruption.
// upstream: if_iwx.c iwx_send_system_statistics_cmd_clear()
pub fn wait_system_statistics_clear<E>(
    wait: &mut SystemStatisticsWait,
    mut sleep_one_second: impl FnMut(&mut SystemStatisticsWait) -> Result<(), E>,
) -> Result<(), E> {
    while !wait.cleared {
        sleep_one_second(wait)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use super::*;

    #[test]
    fn legacy_and_system_statistics_commands_follow_versioned_wire_forms() {
        let legacy = statistics_clear_command(99, 64, 2, 0).unwrap().unwrap();
        assert_eq!(legacy.wire_id, (1 << 8) | u32::from(STATISTICS_COMMAND));
        assert_eq!(legacy.flags, CMD_WANT_RESPONSE);
        assert_eq!(&legacy.bytes[8..12], &STATISTICS_CLEAR.to_le_bytes());
        let modern = statistics_clear_command(1, 0, 2, 0).unwrap().unwrap();
        assert_eq!(
            modern.wire_id,
            (u32::from(SYSTEM_GROUP) << 8) | u32::from(SYSTEM_STATISTICS_COMMAND)
        );
        assert_eq!(modern.flags, CMD_ASYNC);
        assert_eq!(&modern.bytes[8..12], &STATS_CONFIG_ON_DEMAND.to_le_bytes());
        assert_eq!(
            &modern.bytes[16..20],
            &(STATS_TYPE_OPERATIONAL | STATS_TYPE_OPERATIONAL_PART1).to_le_bytes()
        );
        assert_eq!(statistics_clear_command(2, 0, 0, 0), Ok(None));
    }

    #[test]
    fn system_clear_wait_resets_and_waits_until_notification() {
        let mut wait = SystemStatisticsWait { cleared: true };
        begin_system_statistics_clear(&mut wait);
        let calls = Cell::new(0);
        assert_eq!(
            wait_system_statistics_clear(&mut wait, |wait| {
                if calls.get() == 2 {
                    system_statistics_end_notification(wait);
                }
                calls.set(calls.get() + 1);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(calls.get(), 3);
    }
}
