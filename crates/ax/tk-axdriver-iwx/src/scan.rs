//! UMAC scan command and scan-state transitions from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use crate::{CommandError, EncodedCommand, HostCommand};

pub const LONG_GROUP: u8 = 1;
pub const UMAC_SCAN_REQ: u8 = 0x0d;
pub const UMAC_SCAN_ABORT: u8 = 0x0e;
pub const UMAC_SCAN_COMPLETE: u8 = 0x0f;

/// Current driver flags and net80211 state that `iwx_scan()` mutates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanState {
    pub scanning: bool,
    pub background_scan: bool,
    pub in_scan_state: bool,
    pub link_up: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanError<E> {
    AbortBackground(E),
    Initiate(E),
}

/// Build the 8-byte UMAC scan abort command with uid and reserved flags zero.
// upstream: if_iwx.c iwx_umac_scan_abort()
pub fn scan_abort_command(slot: u8, command_queue: u8) -> Result<EncodedCommand, CommandError> {
    let payload = [0u8; 8];
    let command = HostCommand {
        id: (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_ABORT),
        flags: 0,
        response_capacity: 0,
        parts: &[&payload],
    };
    EncodedCommand::encode(&command, slot, command_queue)
}

/// Begin foreground scan, aborting background scan first and retaining source order.
// upstream: if_iwx.c iwx_scan()
pub fn begin_foreground_scan<E>(
    state: &mut ScanState,
    current_mode_auto: bool,
    mut abort_background: impl FnMut() -> Result<(), E>,
    mut initiate_scan: impl FnMut(bool) -> Result<(), E>,
    mut set_mode_auto: impl FnMut(),
    mut set_link_down: impl FnMut(),
    mut cleanup_bss: impl FnMut(),
    mut enter_scan_state: impl FnMut(),
    mut wake_init: impl FnMut(),
) -> Result<(), ScanError<E>> {
    if state.background_scan {
        abort_background().map_err(ScanError::AbortBackground)?;
        state.background_scan = false;
        state.scanning = false;
    }
    initiate_scan(false).map_err(ScanError::Initiate)?;
    if current_mode_auto {
        set_mode_auto();
    }
    state.scanning = true;
    if !state.background_scan {
        set_link_down();
        state.link_up = false;
        cleanup_bss();
        enter_scan_state();
        state.in_scan_state = true;
    }
    wake_init();
    Ok(())
}

/// Begin background scan unless foreground scan is already active.
// upstream: if_iwx.c iwx_bgscan()
pub fn begin_background_scan<E>(
    state: &mut ScanState,
    mut initiate_scan: impl FnMut(bool) -> Result<(), E>,
) -> Result<bool, E> {
    if state.scanning {
        return Ok(false);
    }
    initiate_scan(true)?;
    state.background_scan = true;
    Ok(true)
}

/// Clear scan flags only after firmware confirms the abort command succeeded.
// upstream: if_iwx.c iwx_scan_abort()
pub fn abort_scan<E>(
    state: &mut ScanState,
    mut send_abort: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    send_abort()?;
    state.scanning = false;
    state.background_scan = false;
    Ok(())
}

/// End an active foreground/background scan and call net80211 completion once.
// upstream: if_iwx.c iwx_endscan()
pub fn end_scan(state: &mut ScanState, mut complete_net80211: impl FnMut()) -> bool {
    if !state.scanning && !state.background_scan {
        return false;
    }
    state.scanning = false;
    state.background_scan = false;
    state.in_scan_state = false;
    complete_net80211();
    true
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::cell::RefCell;

    use super::*;

    #[test]
    fn scan_abort_command_has_source_wide_id_and_zero_payload() {
        let command = scan_abort_command(7, 0).unwrap();
        assert_eq!(
            command.wire_id,
            (u32::from(LONG_GROUP) << 8) | u32::from(UMAC_SCAN_ABORT)
        );
        assert_eq!(&command.bytes[8..], &[0; 8]);
    }

    #[test]
    fn foreground_scan_aborts_background_before_initiating_and_updates_state() {
        let events = RefCell::new(Vec::new());
        let mut state = ScanState {
            background_scan: true,
            link_up: true,
            ..ScanState::default()
        };
        begin_foreground_scan(
            &mut state,
            true,
            || {
                events.borrow_mut().push(1);
                Ok::<(), ()>(())
            },
            |background| {
                assert!(!background);
                events.borrow_mut().push(2);
                Ok(())
            },
            || events.borrow_mut().push(3),
            || events.borrow_mut().push(4),
            || events.borrow_mut().push(5),
            || events.borrow_mut().push(6),
            || events.borrow_mut().push(7),
        )
        .unwrap();
        assert_eq!(*events.borrow(), [1, 2, 3, 4, 5, 6, 7]);
        assert!(state.scanning && state.in_scan_state);
        assert!(!state.background_scan && !state.link_up);
    }

    #[test]
    fn scan_abort_failure_keeps_flags_and_completion_clears_once() {
        let mut state = ScanState {
            scanning: true,
            background_scan: true,
            in_scan_state: true,
            link_up: false,
        };
        assert_eq!(abort_scan(&mut state, || Err::<(), _>(1)), Err(1));
        assert!(state.scanning && state.background_scan);
        let mut calls = 0;
        assert!(end_scan(&mut state, || calls += 1));
        assert!(!end_scan(&mut state, || calls += 1));
        assert_eq!(calls, 1);
    }
}
