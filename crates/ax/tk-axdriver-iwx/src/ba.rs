//! RX block-ack sessions and reorder-window state from OpenBSD iwx.
//!
//! Translated from OpenBSD `sys/dev/pci/if_iwx.c` rev 1.230 and
//! `if_iwxreg.h` (ISC). Copyright (c) 2014, 2016 genua gmbh
//! <info@genua.de>; Copyright (c) 2014 Fixup Software Ltd.; Copyright (c)
//! 2017, 2019, 2020 Stefan Sperling <stsp@openbsd.org>.

use alloc::vec::Vec;

use crate::{CMD_WANT_RESPONSE, CommandError, EncodedCommand, HostCommand};

pub const MAX_RX_BA_SESSIONS: usize = 16;
pub const INVALID_BAID: u8 = 0x7f;
pub const STATION_ID: u8 = 0;
pub const RX_REORDER_TIMEOUT_MQ_USEC: u64 = 100_000;
const BAID_GROUP: u8 = 5;
const BAID_CONFIG_COMMAND: u8 = 0x16;
const ADD_STA_COMMAND: u8 = 0x18;
const BAID_ADD: u32 = 0;
const BAID_REMOVE: u32 = 2;
const BAID_MAX: usize = 16;
const BA_WINDOW_MAX: u16 = 64;
const ADD_STA_SUCCESS: u32 = 1;
const ADD_STA_STATUS_MASK: u32 = 0xff;
const ADD_STA_BAID_VALID: u32 = 0x8000;
const ADD_STA_BAID_MASK: u32 = 0x7f00;
const ADD_STA_BAID_SHIFT: u32 = 8;
const STA_MODE_MODIFY: u8 = 1;
const STA_MODIFY_ADD_BA_TID: u8 = 1 << 3;
const STA_MODIFY_REMOVE_BA_TID: u8 = 1 << 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaError {
    InvalidWindow,
    InvalidBaid,
    InvalidResponse,
    NoSession,
    TooManySessions,
    Command(CommandError),
}

impl From<CommandError> for BaError {
    fn from(error: CommandError) -> Self {
        Self::Command(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderBuffer<T> {
    pub head_sn: u16,
    pub num_stored: u16,
    pub buf_size: u16,
    pub valid: bool,
    pub entries: Vec<Vec<T>>,
}

impl<T> ReorderBuffer<T> {
    /// Initialize every sequence-number slot for an RX BA window.
    // upstream: if_iwx.c iwx_init_reorder_buffer()
    pub fn new(ssn: u16, buf_size: u16) -> Result<Self, BaError> {
        if buf_size == 0 || buf_size > BA_WINDOW_MAX {
            return Err(BaError::InvalidWindow);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(usize::from(buf_size))
            .map_err(|_| BaError::InvalidWindow)?;
        for _ in 0..buf_size {
            entries.push(Vec::new());
        }
        Ok(Self {
            head_sn: ssn & 0x0fff,
            num_stored: 0,
            buf_size,
            valid: false,
            entries,
        })
    }

    /// Drop all buffered MPDUs and invalidate the reorder window.
    // upstream: if_iwx.c iwx_clear_reorder_buffer()
    pub fn clear(&mut self) {
        for frames in &mut self.entries {
            frames.clear();
        }
        self.num_stored = 0;
        self.valid = false;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxBaSession<T> {
    pub sta_id: u8,
    pub tid: u8,
    pub baid: u8,
    pub timeout_usec: u16,
    pub last_rx_usec: u64,
    pub reorder: ReorderBuffer<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxBaTable<T> {
    sessions: [Option<RxBaSession<T>>; MAX_RX_BA_SESSIONS],
    pub session_count: usize,
}

impl<T> Default for RxBaTable<T> {
    fn default() -> Self {
        Self {
            sessions: core::array::from_fn(|_| None),
            session_count: 0,
        }
    }
}

impl<T> RxBaTable<T> {
    /// Find an active aggregation session for a traffic identifier.
    // upstream: if_iwx.c iwx_find_rxba_data()
    pub fn find_tid(&self, tid: u8) -> Option<&RxBaSession<T>> {
        self.sessions
            .iter()
            .filter_map(Option::as_ref)
            .find(|session| session.tid == tid)
    }

    pub fn get(&self, baid: u8) -> Option<&RxBaSession<T>> {
        self.sessions.get(usize::from(baid))?.as_ref()
    }

    /// Start an accepted firmware BAID session, preserving its timer inputs.
    // upstream: if_iwx.c iwx_sta_rx_agg()
    pub fn start(
        &mut self,
        baid: u8,
        tid: u8,
        ssn: u16,
        window: u16,
        timeout_usec: u16,
        now_usec: u64,
    ) -> Result<(), BaError> {
        let index = usize::from(baid);
        if index >= self.sessions.len() || baid == INVALID_BAID {
            return Err(BaError::InvalidBaid);
        }
        if self.session_count >= MAX_RX_BA_SESSIONS {
            return Err(BaError::TooManySessions);
        }
        if self.sessions[index].is_some() {
            return Err(BaError::InvalidBaid);
        }
        let reorder = ReorderBuffer::new(ssn, window)?;
        self.sessions[index] = Some(RxBaSession {
            sta_id: STATION_ID,
            tid,
            baid,
            timeout_usec,
            last_rx_usec: now_usec,
            reorder,
        });
        self.session_count += 1;
        Ok(())
    }

    /// Clear a session's buffered frames and retire its BAID.
    // upstream: if_iwx.c iwx_clear_reorder_buffer()
    pub fn clear(&mut self, baid: u8) -> Result<RxBaSession<T>, BaError> {
        let slot = self
            .sessions
            .get_mut(usize::from(baid))
            .ok_or(BaError::InvalidBaid)?;
        let mut session = slot.take().ok_or(BaError::NoSession)?;
        session.reorder.clear();
        session.baid = INVALID_BAID;
        self.session_count = self.session_count.saturating_sub(1);
        Ok(session)
    }

    /// Translate the source's 100 ms RX reorder-session timeout decision.
    // upstream: if_iwx.c iwx_rx_ba_session_expired()
    pub fn timeout_action(
        &self,
        baid: u8,
        now_usec: u64,
        shutdown: bool,
        state_run: bool,
    ) -> Option<BaTimeoutAction> {
        let session = self.get(baid)?;
        if shutdown || !state_run {
            return None;
        }
        if now_usec
            < session
                .last_rx_usec
                .saturating_add(RX_REORDER_TIMEOUT_MQ_USEC)
        {
            Some(BaTimeoutAction::Rearm(session.timeout_usec))
        } else {
            Some(BaTimeoutAction::RequestDelba { tid: session.tid })
        }
    }

    /// Decode and validate a BAR frame-release notification against BAID state.
    // upstream: if_iwx.c iwx_rx_bar_frame_release()
    pub fn parse_bar_release(&self, payload: &[u8]) -> Option<BarFrameRelease> {
        if payload.len() < 8 {
            return None;
        }
        let ba_info = u32::from_le_bytes(payload[..4].try_into().ok()?);
        let sta_tid = u32::from_le_bytes(payload[4..8].try_into().ok()?);
        let baid = ((ba_info & 0x3f00_0000) >> 24) as u8;
        let session = self.get(baid)?;
        let tid = (sta_tid & 0x0f) as u8;
        let sta_id = ((sta_tid & 0x1f0) >> 4) as u8;
        if baid == INVALID_BAID || tid != session.tid || session.sta_id != STATION_ID {
            return None;
        }
        Some(BarFrameRelease {
            baid,
            tid,
            station_id: sta_id,
            nssn: (ba_info & 0x0fff) as u16,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaTimeoutAction {
    Rearm(u16),
    RequestDelba { tid: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarFrameRelease {
    pub baid: u8,
    pub tid: u8,
    pub station_id: u8,
    pub nssn: u16,
}

/// Build the BAID allocation add/remove command and response-bearing header.
// upstream: if_iwx.c iwx_sta_rx_agg_baid_cfg_cmd()
pub fn baid_config_command(
    version: u8,
    tid: u8,
    ssn: u16,
    window: u16,
    baid: Option<u8>,
    slot: u8,
) -> Result<EncodedCommand, BaError> {
    let mut payload = [0u8; 16];
    if let Some(baid) = baid {
        payload[..4].copy_from_slice(&BAID_REMOVE.to_le_bytes());
        if version == 1 {
            payload[4..8].copy_from_slice(&u32::from(baid).to_le_bytes());
        } else {
            payload[4..8].copy_from_slice(&(1u32 << STATION_ID).to_le_bytes());
            payload[8..12].copy_from_slice(&u32::from(tid).to_le_bytes());
        }
    } else {
        payload[..4].copy_from_slice(&BAID_ADD.to_le_bytes());
        payload[4..8].copy_from_slice(&(1u32 << STATION_ID).to_le_bytes());
        payload[8] = tid;
        payload[12..14].copy_from_slice(&(ssn & 0x0fff).to_le_bytes());
        payload[14..16].copy_from_slice(&window.to_le_bytes());
    }
    let id = (u32::from(BAID_GROUP) << 8) | u32::from(BAID_CONFIG_COMMAND);
    let command = HostCommand {
        id,
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Serialize station-table ADD_STA parameters for immediate BA TID add/remove.
// upstream: if_iwx.c iwx_sta_rx_agg_sta_cmd()
pub fn station_ba_command(
    tid: u8,
    ssn: u16,
    window: u16,
    station_color: u32,
    start: bool,
    slot: u8,
) -> Result<EncodedCommand, BaError> {
    let mut payload = [0u8; 48];
    payload[0] = STA_MODE_MODIFY;
    payload[4..8].copy_from_slice(&station_color.to_le_bytes());
    payload[16] = STATION_ID;
    payload[17] = if start {
        STA_MODIFY_ADD_BA_TID
    } else {
        STA_MODIFY_REMOVE_BA_TID
    };
    if start {
        payload[28] = tid;
        payload[30..32].copy_from_slice(&(ssn & 0x0fff).to_le_bytes());
        payload[44..46].copy_from_slice(&window.to_le_bytes());
    } else {
        payload[29] = tid;
    }
    let command = HostCommand {
        id: u32::from(ADD_STA_COMMAND),
        flags: CMD_WANT_RESPONSE,
        response_capacity: 8,
        parts: &[&payload],
    };
    Ok(EncodedCommand::encode(&command, slot, 0)?)
}

/// Validate the ADD_STA success, BAID-valid bit, and returned BAID range.
// upstream: if_iwx.c iwx_sta_rx_agg_sta_cmd()
pub fn station_ba_response(response: &[u8]) -> Result<u8, BaError> {
    let status = u32::from_le_bytes(
        response
            .get(..4)
            .ok_or(BaError::InvalidResponse)?
            .try_into()
            .unwrap(),
    );
    if status & ADD_STA_STATUS_MASK != ADD_STA_SUCCESS || status & ADD_STA_BAID_VALID == 0 {
        return Err(BaError::InvalidResponse);
    }
    let baid = ((status & ADD_STA_BAID_MASK) >> ADD_STA_BAID_SHIFT) as u8;
    if baid == INVALID_BAID || usize::from(baid) >= BAID_MAX {
        return Err(BaError::InvalidBaid);
    }
    Ok(baid)
}

/// Validate the u32 BAID returned by RX_BAID_ALLOCATION_CONFIG_CMD.
// upstream: if_iwx.c iwx_sta_rx_agg_baid_cfg_cmd()
pub fn baid_config_response(response: &[u8]) -> Result<u8, BaError> {
    let baid = u32::from_le_bytes(
        response
            .get(..4)
            .ok_or(BaError::InvalidResponse)?
            .try_into()
            .unwrap(),
    );
    if baid as usize >= BAID_MAX {
        Err(BaError::InvalidBaid)
    } else {
        Ok(baid as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reorder_state_and_timeout_follow_iwx_lifecycle() {
        let mut table = RxBaTable::<u8>::default();
        table.start(3, 5, 0x1fff, 64, 20_000, 10).unwrap();
        let session = table.find_tid(5).unwrap();
        assert_eq!(session.reorder.head_sn, 0x0fff);
        assert_eq!(session.reorder.entries.len(), 64);
        assert_eq!(
            table.timeout_action(3, 50_000, false, true),
            Some(BaTimeoutAction::Rearm(20_000))
        );
        assert_eq!(
            table.timeout_action(3, 100_010, false, true),
            Some(BaTimeoutAction::RequestDelba { tid: 5 })
        );
        assert_eq!(table.timeout_action(3, 100_010, true, true), None);
        let retired = table.clear(3).unwrap();
        assert_eq!(retired.baid, INVALID_BAID);
        assert_eq!(table.session_count, 0);
    }

    #[test]
    fn bar_release_matches_active_baid_and_tid() {
        let mut table = RxBaTable::<u8>::default();
        table.start(2, 7, 0, 16, 0, 0).unwrap();
        let ba_info = (2u32 << 24) | 0x123;
        let sta_tid = (u32::from(STATION_ID) << 4) | 7;
        let mut payload = [0; 8];
        payload[..4].copy_from_slice(&ba_info.to_le_bytes());
        payload[4..].copy_from_slice(&sta_tid.to_le_bytes());
        assert_eq!(
            table.parse_bar_release(&payload),
            Some(BarFrameRelease {
                baid: 2,
                tid: 7,
                station_id: 0,
                nssn: 0x123,
            })
        );
        payload[4] = 6;
        assert_eq!(table.parse_bar_release(&payload), None);
    }

    #[test]
    fn baid_and_station_commands_validate_response_and_wire_offsets() {
        let add = baid_config_command(2, 3, 0xabc, 32, None, 4).unwrap();
        assert_eq!(add.flags, CMD_WANT_RESPONSE);
        assert_eq!(add.response_capacity, 8);
        assert_eq!(&add.bytes[8..12], &BAID_ADD.to_le_bytes());
        assert_eq!(&add.bytes[12..16], &1u32.to_le_bytes());
        assert_eq!(add.bytes[16], 3);
        assert_eq!(&add.bytes[20..22], &0xabcu16.to_le_bytes());
        assert_eq!(&add.bytes[22..24], &32u16.to_le_bytes());
        let remove = baid_config_command(1, 3, 0, 0, Some(4), 4).unwrap();
        assert_eq!(&remove.bytes[12..16], &4u32.to_le_bytes());

        let station = station_ba_command(3, 0xabc, 32, 0x1234, true, 2).unwrap();
        assert_eq!(station.bytes.len(), 56);
        assert_eq!(station.bytes[8], STA_MODE_MODIFY);
        assert_eq!(station.bytes[24], STATION_ID);
        assert_eq!(station.bytes[36], 3);
        assert_eq!(&station.bytes[38..40], &0xabcu16.to_le_bytes());
        let status =
            (ADD_STA_BAID_VALID | (4 << ADD_STA_BAID_SHIFT) | ADD_STA_SUCCESS).to_le_bytes();
        assert_eq!(station_ba_response(&status), Ok(4));
        assert_eq!(baid_config_response(&4u32.to_le_bytes()), Ok(4));
    }
}
