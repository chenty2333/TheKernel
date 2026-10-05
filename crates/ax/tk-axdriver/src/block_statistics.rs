//! Allocation-free observations of actual native block publication/completion.
//! This ledger never admits, retries, drains or cancels a device request.
use axsync::spin::SpinNoIrq;

use crate::prelude::{
    BlockAsyncOp, BlockCompletion, BlockCompletionOwner, BlockCompletionStatus,
    BlockPhysicalRequest, BlockQueueRequest, BlockSubmitReport, DevResult,
};

#[derive(Clone, Copy, Debug)]
pub enum Operation {
    Read,
    Write,
    Discard,
    Flush,
}
impl Operation {
    const fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Discard => 2,
            Self::Flush => 3,
        }
    }
}
impl From<BlockAsyncOp> for Operation {
    fn from(op: BlockAsyncOp) -> Self {
        match op {
            BlockAsyncOp::Read => Self::Read,
            BlockAsyncOp::Write => Self::Write,
            BlockAsyncOp::Flush => Self::Flush,
        }
    }
}

#[derive(Clone, Copy)]
pub struct LegacyTicket {
    op: Operation,
    start: u64,
}
#[derive(Clone, Copy)]
struct Pending {
    raw: u64,
    cookie: Option<u64>,
    op: Operation,
    bytes: u64,
    start: u64,
}

struct Ledger<const N: usize> {
    count: [u64; 4],
    bytes: [u64; 4],
    time: [u128; 4],
    inflight: u64,
    busy: u128,
    last: u64,
    valid: bool,
    pending: [Option<Pending>; N],
}
impl<const N: usize> Ledger<N> {
    const fn new() -> Self {
        Self {
            count: [0; 4],
            bytes: [0; 4],
            time: [0; 4],
            inflight: 0,
            busy: 0,
            last: 0,
            valid: true,
            pending: [None; N],
        }
    }
    fn advance(&mut self, now: u64) {
        if self.inflight != 0 {
            self.busy = self
                .busy
                .saturating_add(u128::from(now.saturating_sub(self.last)));
        }
        self.last = now;
    }
    fn begin(&mut self, op: Operation, now: u64) -> LegacyTicket {
        self.advance(now);
        self.inflight = self.inflight.saturating_add(1);
        LegacyTicket { op, start: now }
    }
    fn finish(&mut self, ticket: LegacyTicket, bytes: Option<u64>, now: u64) {
        self.advance(now);
        self.inflight = self.inflight.saturating_sub(1);
        if let Some(bytes) = bytes {
            let index = ticket.op.index();
            self.count[index] = self.count[index].saturating_add(1);
            self.bytes[index] = self.bytes[index].saturating_add(bytes);
            self.time[index] =
                self.time[index].saturating_add(u128::from(now.saturating_sub(ticket.start)));
        }
    }
    fn publish(&mut self, raw: u64, cookie: Option<u64>, op: Operation, bytes: u64, now: u64) {
        if self.pending.iter().flatten().any(|entry| entry.raw == raw) {
            self.valid = false;
            return;
        }
        let Some(index) = self.pending.iter().position(Option::is_none) else {
            self.valid = false;
            return;
        };
        let ticket = self.begin(op, now);
        self.pending[index] = Some(Pending {
            raw,
            cookie,
            op,
            bytes,
            start: ticket.start,
        });
    }
    fn complete(&mut self, record: BlockCompletion, now: u64) {
        let cookie = if record.owner == BlockCompletionOwner::Physical {
            Some(record.cookie)
        } else {
            None
        };
        let Some(index) = self.pending.iter().position(|entry| {
            entry.is_some_and(|entry| entry.raw == record.handle.raw && entry.cookie == cookie)
        }) else {
            self.valid = false;
            return;
        };
        // A quarantine status does not prove DMA retirement. Keep it pending.
        if record.status == BlockCompletionStatus::Quarantined {
            return;
        }
        let entry = self.pending[index].take().unwrap();
        self.finish(
            LegacyTicket {
                op: entry.op,
                start: entry.start,
            },
            (record.status == BlockCompletionStatus::Success).then_some(entry.bytes),
            now,
        );
    }
    fn cancel_quiesced(&mut self, now: u64) {
        for index in 0..N {
            if let Some(entry) = self.pending[index].take() {
                self.finish(
                    LegacyTicket {
                        op: entry.op,
                        start: entry.start,
                    },
                    None,
                    now,
                );
            }
        }
    }
    fn snapshot(&self, now: u64) -> Option<Snapshot> {
        if !self.valid {
            return None;
        }
        let millis = |value: u128| (value / 1_000_000).min(u128::from(u64::MAX)) as u64;
        let busy = self.busy.saturating_add(if self.inflight != 0 {
            u128::from(now.saturating_sub(self.last))
        } else {
            0
        });
        let weighted = self
            .time
            .iter()
            .fold(0u128, |sum, time| sum.saturating_add(*time));
        Some(Snapshot {
            fields: [
                self.count[0],
                0,
                self.bytes[0] / 512,
                millis(self.time[0]),
                self.count[1],
                0,
                self.bytes[1] / 512,
                millis(self.time[1]),
                self.inflight,
                millis(busy),
                millis(weighted),
                self.count[2],
                0,
                self.bytes[2] / 512,
                millis(self.time[2]),
                self.count[3],
                millis(self.time[3]),
            ],
        })
    }
}

/// Linux field ordering; merge counters are zero because this owner never
/// merges separate requests. Times measure native observation intervals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub fields: [u64; 17],
}

pub struct Statistics<const N: usize = 128> {
    ledger: SpinNoIrq<Ledger<N>>,
}
impl<const N: usize> Default for Statistics<N> {
    fn default() -> Self {
        Self::new()
    }
}
impl<const N: usize> Statistics<N> {
    pub const fn new() -> Self {
        Self {
            ledger: SpinNoIrq::new(Ledger::new()),
        }
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        let ledger = self.ledger.lock();
        ledger.snapshot(axhal::time::monotonic_time_nanos())
    }
    pub fn begin_legacy(&self, op: Operation) -> LegacyTicket {
        let mut ledger = self.ledger.lock();
        ledger.begin(op, axhal::time::monotonic_time_nanos())
    }
    pub fn finish_legacy(&self, ticket: LegacyTicket, bytes: Option<u64>) {
        let mut ledger = self.ledger.lock();
        ledger.finish(ticket, bytes, axhal::time::monotonic_time_nanos());
    }
    pub(crate) fn legacy<R>(
        &self,
        op: Operation,
        bytes: Option<u64>,
        operation: impl FnOnce() -> DevResult<R>,
    ) -> DevResult<R> {
        let ticket = self.begin_legacy(op);
        let result = operation();
        if result.is_ok() && bytes.is_none() {
            self.ledger.lock().valid = false;
        }
        self.finish_legacy(ticket, if result.is_ok() { bytes } else { None });
        result
    }
    pub(crate) fn queue(&self, requests: &[BlockQueueRequest<'_>], report: BlockSubmitReport) {
        let mut ledger = self.ledger.lock();
        let now = axhal::time::monotonic_time_nanos();
        if report.submitted > requests.len() {
            ledger.valid = false;
            return;
        }
        for request in requests.iter().take(report.submitted) {
            let Some(handle) = request.handle else {
                ledger.valid = false;
                continue;
            };
            let Some(bytes) = request
                .segments
                .iter()
                .try_fold(0u64, |sum, segment| sum.checked_add(segment.len as u64))
            else {
                ledger.valid = false;
                continue;
            };
            ledger.publish(handle.raw, None, request.op.into(), bytes, now);
        }
    }
    pub(crate) fn physical(
        &self,
        requests: &[BlockPhysicalRequest<'_>],
        report: BlockSubmitReport,
    ) {
        let mut ledger = self.ledger.lock();
        let now = axhal::time::monotonic_time_nanos();
        if report.submitted > requests.len() {
            ledger.valid = false;
            return;
        }
        for request in requests.iter().take(report.submitted) {
            let (Some(handle), Some(cookie)) = (request.handle, request.cookie) else {
                ledger.valid = false;
                continue;
            };
            let Some(bytes) = request
                .segments
                .iter()
                .try_fold(0u64, |sum, segment| sum.checked_add(segment.len as u64))
            else {
                ledger.valid = false;
                continue;
            };
            ledger.publish(handle.raw, Some(cookie), request.op.into(), bytes, now);
        }
    }
    pub(crate) fn completed(&self, records: &[BlockCompletion]) {
        let mut ledger = self.ledger.lock();
        let now = axhal::time::monotonic_time_nanos();
        for record in records {
            ledger.complete(*record, now);
        }
    }
    pub(crate) fn quiesced(&self) {
        let mut ledger = self.ledger.lock();
        let now = axhal::time::monotonic_time_nanos();
        ledger.cancel_quiesced(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::BlockRequestHandle;
    fn completion(raw: u64, cookie: u64, status: BlockCompletionStatus) -> BlockCompletion {
        BlockCompletion {
            handle: BlockRequestHandle { raw },
            owner: BlockCompletionOwner::Physical,
            cookie,
            status,
            bytes: 1,
        }
    }
    #[test]
    fn publication_inflight_completion_and_read_snapshot_do_not_double_count() {
        let mut ledger = Ledger::<4>::new();
        ledger.publish(1, Some(11), Operation::Read, 4096, 1_000_000);
        ledger.publish(2, Some(12), Operation::Write, 1024, 2_000_000);
        let snapshot = ledger.snapshot(3_000_000).unwrap();
        assert_eq!(snapshot.fields[8], 2);
        assert_eq!(snapshot.fields[9], 2);
        assert_eq!(ledger.last, 2_000_000);
        assert_eq!(ledger.count, [0; 4]);
        ledger.complete(completion(1, 11, BlockCompletionStatus::Success), 4_000_000);
        ledger.complete(completion(2, 12, BlockCompletionStatus::Success), 6_000_000);
        assert_eq!(
            ledger.snapshot(20_000_000).unwrap().fields,
            [1, 0, 8, 3, 1, 0, 2, 4, 0, 5, 7, 0, 0, 0, 0, 0, 0]
        );
    }
    #[test]
    fn errors_quarantine_reset_and_unknown_identity_are_not_fake_successes() {
        let mut ledger = Ledger::<2>::new();
        ledger.publish(1, Some(11), Operation::Read, 512, 0);
        ledger.complete(
            completion(1, 11, BlockCompletionStatus::Quarantined),
            1_000_000,
        );
        assert_eq!(ledger.snapshot(2_000_000).unwrap().fields[8], 1);
        ledger.cancel_quiesced(3_000_000);
        assert_eq!(ledger.snapshot(4_000_000).unwrap().fields[0], 0);
        assert_eq!(ledger.snapshot(4_000_000).unwrap().fields[8], 0);
        ledger.complete(completion(1, 11, BlockCompletionStatus::Success), 5_000_000);
        assert!(ledger.snapshot(5_000_000).is_none());
    }
    #[test]
    fn short_legacy_bytes_and_flush_discard_have_real_units() {
        let mut ledger = Ledger::<0>::new();
        for bytes in [128, 128, 256] {
            let ticket = ledger.begin(Operation::Read, 0);
            ledger.finish(ticket, Some(bytes), 1_000_000);
        }
        let ticket = ledger.begin(Operation::Flush, 2_000_000);
        ledger.finish(ticket, Some(0), 3_000_000);
        let ticket = ledger.begin(Operation::Discard, 4_000_000);
        ledger.finish(ticket, Some(4096), 6_000_000);
        let snapshot = ledger.snapshot(7_000_000).unwrap();
        assert_eq!(snapshot.fields[0], 3);
        assert_eq!(snapshot.fields[2], 1);
        assert_eq!(&snapshot.fields[11..], [1, 0, 8, 2, 1, 1]);
    }
}
