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
pub const MAX_TID_COUNT: usize = 16;
pub const FIXED_TX_AGGREGATION_WINDOW: u16 = 64;
const BAID_GROUP: u8 = 5;
const BAID_CONFIG_COMMAND: u8 = 0x16;
const ADD_STA_COMMAND: u8 = 0x18;
const BAID_ADD: u32 = 0;
const BAID_REMOVE: u32 = 2;
const BAID_MAX: usize = 16;
const BA_WINDOW_MAX: u16 = 64;
const REORDER_SEQUENCE_MASK: u16 = 0x0fff;
const REORDER_SEQUENCE_HALF: u16 = 0x0800;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmpduRequestError {
    NoSpace,
    Busy,
    Unsupported,
    InvalidTid,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BaTaskRequests {
    pub shutdown: bool,
    pub rx_start_tid_mask: u16,
    pub rx_stop_tid_mask: u16,
    pub tx_start_tid_mask: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaTaskAction {
    RxStart,
    RxStop,
    TxStart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmpduDisposition {
    Pending,
    Unchanged,
}

/// Queue an RX ADDBA operation for deferred net80211-task context.
// upstream: if_iwx.c iwx_ampdu_rx_start()
pub fn ampdu_rx_start(
    requests: &mut BaTaskRequests,
    active_sessions: usize,
    tid: u8,
    mut enqueue_task: impl FnMut(),
) -> Result<AmpduDisposition, AmpduRequestError> {
    if active_sessions >= MAX_RX_BA_SESSIONS || usize::from(tid) >= MAX_TID_COUNT {
        return Err(AmpduRequestError::NoSpace);
    }
    let bit = 1u16 << tid;
    if requests.rx_start_tid_mask & bit != 0 {
        return Err(AmpduRequestError::Busy);
    }
    requests.rx_start_tid_mask |= bit;
    enqueue_task();
    Ok(AmpduDisposition::Pending)
}

/// Queue RX DELBA teardown unless the TID is invalid or already pending.
// upstream: if_iwx.c iwx_ampdu_rx_stop()
pub fn ampdu_rx_stop(
    requests: &mut BaTaskRequests,
    tid: u8,
    mut enqueue_task: impl FnMut(),
) -> AmpduDisposition {
    if usize::from(tid) >= MAX_TID_COUNT {
        return AmpduDisposition::Unchanged;
    }
    let bit = 1u16 << tid;
    if requests.rx_stop_tid_mask & bit != 0 {
        return AmpduDisposition::Unchanged;
    }
    requests.rx_stop_tid_mask |= bit;
    enqueue_task();
    AmpduDisposition::Pending
}

/// Queue TX ADDBA only for DQA firmware and the driver's fixed window size.
// upstream: if_iwx.c iwx_ampdu_tx_start()
pub fn ampdu_tx_start(
    requests: &mut BaTaskRequests,
    first_data_queue: u8,
    dqa_command_queue: u8,
    tid: u8,
    window_size: u16,
    aggregation_queue: u8,
    mut enqueue_task: impl FnMut(),
) -> Result<AmpduDisposition, AmpduRequestError> {
    if first_data_queue != dqa_command_queue.wrapping_add(1) {
        return Err(AmpduRequestError::Unsupported);
    }
    if usize::from(tid) >= MAX_TID_COUNT {
        return Err(AmpduRequestError::InvalidTid);
    }
    if window_size != FIXED_TX_AGGREGATION_WINDOW {
        return Err(AmpduRequestError::Unsupported);
    }
    if aggregation_queue != 0 {
        return Err(AmpduRequestError::NoSpace);
    }
    let bit = 1u16 << tid;
    if requests.tx_start_tid_mask & bit != 0 {
        return Err(AmpduRequestError::Busy);
    }
    requests.tx_start_tid_mask |= bit;
    enqueue_task();
    Ok(AmpduDisposition::Pending)
}

/// Process the source RX-start/RX-stop per-TID queue, then TX-start queue.
// upstream: if_iwx.c iwx_ba_task()
pub fn run_ba_task(
    requests: &mut BaTaskRequests,
    mut apply: impl FnMut(BaTaskAction, u8),
) -> usize {
    let mut processed = 0;
    for tid in 0..MAX_TID_COUNT {
        if requests.shutdown {
            break;
        }
        let bit = 1u16 << tid;
        if requests.rx_start_tid_mask & bit != 0 {
            apply(BaTaskAction::RxStart, tid as u8);
            requests.rx_start_tid_mask &= !bit;
            processed += 1;
        } else if requests.rx_stop_tid_mask & bit != 0 {
            apply(BaTaskAction::RxStop, tid as u8);
            requests.rx_stop_tid_mask &= !bit;
            processed += 1;
        }
    }
    for tid in 0..MAX_TID_COUNT {
        if requests.shutdown {
            break;
        }
        let bit = 1u16 << tid;
        if requests.tx_start_tid_mask & bit != 0 {
            apply(BaTaskAction::TxStart, tid as u8);
            requests.tx_start_tid_mask &= !bit;
            processed += 1;
        }
    }
    processed
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

#[derive(Debug, PartialEq, Eq)]
pub struct RxReorderOutcome<T> {
    /// Frames released in source order, or the current frame on passthrough.
    pub frames: Vec<T>,
    /// False means the current frame bypassed reorder and `frames[0]` is current.
    pub consumed: bool,
    pub dropped: bool,
    pub ampdu_done: bool,
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

    /// Buffer, release, bypass, or drop one validated firmware RX reorder entry.
    // upstream: if_iwx.c iwx_rx_reorder()
    pub fn reorder_mpdu(
        &mut self,
        baid: u8,
        tid: u8,
        sequence_number: u16,
        nssn: u16,
        amsdu: bool,
        last_amsdu_subframe: bool,
        old_sequence: bool,
        duplicate: bool,
        now_usec: u64,
        frame: T,
    ) -> Result<Option<RxReorderOutcome<T>>, BaError> {
        let Some(session) = self
            .sessions
            .get_mut(usize::from(baid))
            .and_then(Option::as_mut)
        else {
            return Ok(None);
        };
        if session.baid != baid || session.sta_id != STATION_ID || session.tid != tid {
            return Ok(None);
        }
        if session.timeout_usec != 0 {
            session.last_rx_usec = now_usec;
        }
        let reorder = &mut session.reorder;
        if !reorder.valid {
            if old_sequence {
                return Ok(None);
            }
            reorder.valid = true;
        }
        if duplicate || old_sequence {
            return Ok(Some(RxReorderOutcome {
                frames: Vec::new(),
                consumed: true,
                dropped: true,
                ampdu_done: true,
            }));
        }
        let sequence_number = sequence_number & REORDER_SEQUENCE_MASK;
        let nssn = nssn & REORDER_SEQUENCE_MASK;
        if reorder.num_stored == 0 && seq_less_than(sequence_number, nssn) {
            if !amsdu || last_amsdu_subframe {
                reorder.head_sn = nssn;
            }
            let mut frames = Vec::new();
            frames
                .try_reserve_exact(1)
                .map_err(|_| BaError::InvalidWindow)?;
            frames.push(frame);
            return Ok(Some(RxReorderOutcome {
                frames,
                consumed: false,
                dropped: false,
                ampdu_done: true,
            }));
        }
        if reorder.num_stored == 0 && sequence_number == reorder.head_sn {
            if !amsdu || last_amsdu_subframe {
                reorder.head_sn = reorder.head_sn.wrapping_add(1) & REORDER_SEQUENCE_MASK;
            }
            let mut frames = Vec::new();
            frames
                .try_reserve_exact(1)
                .map_err(|_| BaError::InvalidWindow)?;
            frames.push(frame);
            return Ok(Some(RxReorderOutcome {
                frames,
                consumed: false,
                dropped: false,
                ampdu_done: true,
            }));
        }
        let index = usize::from(sequence_number % reorder.buf_size);
        reorder.entries[index]
            .try_reserve(1)
            .map_err(|_| BaError::InvalidWindow)?;
        reorder.entries[index].push(frame);
        reorder.num_stored = reorder.num_stored.saturating_add(1);
        let mut frames = Vec::new();
        if !amsdu || last_amsdu_subframe {
            frames = release_frames(reorder, nssn)?;
        }
        Ok(Some(RxReorderOutcome {
            frames,
            consumed: true,
            dropped: false,
            ampdu_done: true,
        }))
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

/// Release all buffered sequence slots before firmware's next sequence number.
// upstream: if_iwx.c iwx_release_frames()
fn release_frames<T>(reorder: &mut ReorderBuffer<T>, nssn: u16) -> Result<Vec<T>, BaError> {
    let mut frames = Vec::new();
    while seq_less_than(reorder.head_sn, nssn) {
        let slot = usize::from(reorder.head_sn % reorder.buf_size);
        let released = core::mem::take(&mut reorder.entries[slot]);
        frames
            .try_reserve(released.len())
            .map_err(|_| BaError::InvalidWindow)?;
        reorder.num_stored = reorder
            .num_stored
            .saturating_sub(released.len().min(usize::from(u16::MAX)) as u16);
        frames.extend(released);
        reorder.head_sn = reorder.head_sn.wrapping_add(1) & REORDER_SEQUENCE_MASK;
    }
    Ok(frames)
}

fn seq_less_than(left: u16, right: u16) -> bool {
    let difference = left.wrapping_sub(right) & REORDER_SEQUENCE_MASK;
    difference & REORDER_SEQUENCE_HALF != 0
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
    fn ampdu_callbacks_queue_source_tid_masks_and_task_order() {
        let mut requests = BaTaskRequests::default();
        let mut queued = 0;
        assert_eq!(
            ampdu_rx_start(&mut requests, 0, 3, || queued += 1),
            Ok(AmpduDisposition::Pending)
        );
        assert_eq!(
            ampdu_rx_start(&mut requests, 0, 3, || queued += 1),
            Err(AmpduRequestError::Busy)
        );
        assert_eq!(
            ampdu_rx_stop(&mut requests, 3, || queued += 1),
            AmpduDisposition::Pending
        );
        assert_eq!(
            ampdu_tx_start(&mut requests, 1, 0, 5, 64, 0, || queued += 1),
            Ok(AmpduDisposition::Pending)
        );
        assert_eq!(queued, 3);
        let mut actions = Vec::new();
        assert_eq!(
            run_ba_task(&mut requests, |action, tid| actions.push((action, tid))),
            2
        );
        assert_eq!(
            actions,
            [(BaTaskAction::RxStart, 3), (BaTaskAction::TxStart, 5)]
        );
        assert_eq!(requests.rx_stop_tid_mask, 1 << 3);
        assert_eq!(
            run_ba_task(&mut requests, |action, tid| actions.push((action, tid))),
            1
        );
        assert_eq!(actions.last(), Some(&(BaTaskAction::RxStop, 3)));
    }

    #[test]
    fn ampdu_tx_gates_and_shutdown_leave_requests_pending() {
        let mut requests = BaTaskRequests::default();
        assert_eq!(
            ampdu_tx_start(&mut requests, 0, 0, 0, 64, 0, || {}),
            Err(AmpduRequestError::Unsupported)
        );
        assert_eq!(
            ampdu_tx_start(&mut requests, 1, 0, 16, 64, 0, || {}),
            Err(AmpduRequestError::InvalidTid)
        );
        assert_eq!(
            ampdu_tx_start(&mut requests, 1, 0, 0, 32, 0, || {}),
            Err(AmpduRequestError::Unsupported)
        );
        ampdu_rx_start(&mut requests, 0, 1, || {}).unwrap();
        requests.shutdown = true;
        assert_eq!(
            run_ba_task(&mut requests, |_, _| panic!("shutdown must not dispatch")),
            0
        );
        assert_eq!(requests.rx_start_tid_mask, 1 << 1);
    }

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
    fn reorder_mpdu_handles_wrap_gap_duplicates_and_amsdu_nssn() {
        let mut table = RxBaTable::<u8>::default();
        table.start(1, 3, 0x0ffe, 64, 20_000, 0).unwrap();
        let buffered = table
            .reorder_mpdu(1, 3, 0, 0x0fff, false, true, false, false, 1, 10)
            .unwrap()
            .unwrap();
        assert!(buffered.consumed && buffered.frames.is_empty());
        let released = table
            .reorder_mpdu(1, 3, 0x0fff, 1, false, true, false, false, 2, 11)
            .unwrap()
            .unwrap();
        assert_eq!(released.frames, [11, 10]);
        assert_eq!(table.get(1).unwrap().reorder.head_sn, 1);

        let duplicate = table
            .reorder_mpdu(1, 3, 1, 2, false, true, false, true, 3, 12)
            .unwrap()
            .unwrap();
        assert!(duplicate.consumed && duplicate.dropped);
        assert!(duplicate.ampdu_done);

        let mut amsdu = RxBaTable::<u8>::default();
        amsdu.start(2, 4, 0, 16, 0, 0).unwrap();
        let first_subframe = amsdu
            .reorder_mpdu(2, 4, 0, 1, true, false, false, false, 1, 20)
            .unwrap()
            .unwrap();
        assert!(!first_subframe.consumed);
        assert_eq!(first_subframe.frames, [20]);
        assert_eq!(amsdu.get(2).unwrap().reorder.head_sn, 0);
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
