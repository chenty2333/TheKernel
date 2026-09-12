//! Bounded io_uring diagnostics; not a perf transport or a general ftrace tracer.
//! Producers never allocate, format, print, or wait. Full captures stop recording
//! until explicitly cleared. Readers copy under the lock and format afterwards.

use alloc::{string::String, vec::Vec};
use core::{
    fmt::{self, Write},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use axsync::spin::SpinNoIrq;
use thekernel_linux_io_uring::{RequestId, RequestTraceEvent};

const CAPACITY: usize = 1024;
static ENABLED: AtomicBool = AtomicBool::new(false);
static SEQUENCE: AtomicU64 = AtomicU64::new(0);
// Cumulative drops never reset: producers use one bounded fetch_add. Clear
// samples a baseline under the capture lock for the next snapshot.
static DROPPED: AtomicU64 = AtomicU64::new(0);
static CAPTURE: SpinNoIrq<Capture> = SpinNoIrq::new(Capture::new());

#[derive(Clone, Copy)]
enum CaptureEvent {
    Lifecycle(RequestTraceEvent),
    ExecutorStarted(RequestId),
    ExecutorReturned(RequestId),
    ReadStage(RequestId, ReadStage),
}

/// Diagnostic boundaries for the synchronous registered-buffer read target.
#[derive(Clone, Copy)]
pub(crate) enum ReadStage {
    PermissionStarted,
    PermissionReturned,
    ReadStarted,
    ReadReturned,
    NotificationReturned,
}

impl ReadStage {
    fn name(self) -> &'static str {
        match self {
            Self::PermissionStarted => "permission_started",
            Self::PermissionReturned => "permission_returned",
            Self::ReadStarted => "read_started",
            Self::ReadReturned => "read_returned",
            Self::NotificationReturned => "notification_returned",
        }
    }
}

pub(crate) fn read_stage(id: Option<RequestId>, stage: ReadStage) {
    if let Some(id) = id {
        record_event(CaptureEvent::ReadStage(id, stage));
    }
}

#[derive(Clone, Copy)]
struct Record {
    sequence: u64,
    nanos: u64,
    event: CaptureEvent,
}
struct Capture {
    records: [Option<Record>; CAPACITY],
    len: usize,
    dropped_base: u64,
}
impl Capture {
    const fn new() -> Self {
        Self {
            records: [None; CAPACITY],
            len: 0,
            dropped_base: 0,
        }
    }
    fn push(&mut self, record: Record) -> bool {
        if self.len == CAPACITY {
            return false;
        }
        self.records[self.len] = Some(record);
        self.len += 1;
        true
    }
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}
pub(super) fn set_enabled(value: bool) {
    let _capture = CAPTURE.lock();
    ENABLED.store(value, Ordering::Relaxed);
}
pub(super) fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

pub(crate) fn record(event: RequestTraceEvent) {
    record_event(CaptureEvent::Lifecycle(event));
}

pub(crate) fn executor_started(id: RequestId) {
    record_event(CaptureEvent::ExecutorStarted(id));
}

pub(crate) fn executor_returned(id: RequestId) {
    record_event(CaptureEvent::ExecutorReturned(id));
}

fn record_event(event: CaptureEvent) {
    if !enabled() {
        return;
    }
    let Some(mut capture) = CAPTURE.try_lock() else {
        SEQUENCE.fetch_add(1, Ordering::Relaxed);
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    // Pair control writes with the capture lock: disabling waits for an
    // active writer and prevents a delayed precheck from recording afterwards.
    if !enabled() {
        return;
    }
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let record = Record {
        sequence,
        nanos: axhal::time::monotonic_time_nanos(),
        event,
    };
    if !capture.push(record) {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn clear() {
    let mut capture = CAPTURE.lock();
    capture.len = 0;
    capture.dropped_base = dropped();
}

pub(super) fn snapshot() -> Result<String, axfs_ng_vfs::VfsError> {
    let mut records = Vec::new();
    records
        .try_reserve_exact(CAPACITY)
        .map_err(|_| axfs_ng_vfs::VfsError::NoMemory)?;
    let dropped;
    let dropped_total;
    {
        let capture = CAPTURE.lock();
        records.extend(capture.records[..capture.len].iter().flatten().copied());
        dropped_total = self::dropped();
        dropped = dropped_total.wrapping_sub(capture.dropped_base);
    }
    format_snapshot(&records, dropped, dropped_total).map_err(|_| axfs_ng_vfs::VfsError::NoMemory)
}

// Reserve before every append, including fragments emitted by Debug. A failed
// allocation returns an error instead of aborting while reading diagnostics.
#[derive(Default)]
struct SnapshotText(String);
impl Write for SnapshotText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.push_str(text);
        Ok(())
    }
}

fn format_snapshot(
    records: &[Record],
    dropped: u64,
    dropped_total: u64,
) -> Result<String, fmt::Error> {
    let mut output = SnapshotText::default();
    write!(
        output,
        "# io_uring lifecycle snapshot; capacity={CAPACITY} policy=stop-on-full dropped={dropped} \
         dropped_total={dropped_total}\n"
    )?;
    for Record {
        sequence,
        nanos,
        event,
    } in records
    {
        write!(output, "seq={sequence} ns={nanos} ")?;
        let event = match event {
            CaptureEvent::Lifecycle(event) => event,
            CaptureEvent::ExecutorStarted(id) => {
                simple(&mut output, *id, "executor_started")?;
                continue;
            }
            CaptureEvent::ExecutorReturned(id) => {
                simple(&mut output, *id, "executor_returned")?;
                continue;
            }
            CaptureEvent::ReadStage(id, stage) => {
                simple(&mut output, *id, stage.name())?;
                continue;
            }
        };
        match event {
            RequestTraceEvent::Reserved { id, descriptor } => {
                identity(&mut output, *id)?;
                writeln!(
                    output,
                    "event=reserved user_data={} operation={:?}",
                    descriptor.user_data(),
                    descriptor.operation()
                )?;
            }
            RequestTraceEvent::Submitted { id } => simple(&mut output, *id, "submitted")?,
            RequestTraceEvent::Issued { id } => simple(&mut output, *id, "issued")?,
            RequestTraceEvent::RolledBack { id } => simple(&mut output, *id, "rolled_back")?,
            RequestTraceEvent::Discarded { id } => simple(&mut output, *id, "discarded")?,
            RequestTraceEvent::CompletionAccepted {
                id,
                cause,
                result,
                flags,
            } => {
                identity(&mut output, *id)?;
                writeln!(
                    output,
                    "event=completion_accepted cause={cause:?} result={result} flags={flags}"
                )?;
            }
            RequestTraceEvent::PublicationStarted { id, terminal } => {
                identity(&mut output, *id)?;
                writeln!(output, "event=publication_started terminal={terminal}")?;
            }
            RequestTraceEvent::PublicationRolledBack { id, terminal } => {
                identity(&mut output, *id)?;
                writeln!(output, "event=publication_rolled_back terminal={terminal}")?;
            }
            RequestTraceEvent::ProviderCancelSelected { id } => {
                simple(&mut output, *id, "provider_cancel_selected")?
            }
            RequestTraceEvent::ProviderCancelResolved { id, outcome } => {
                identity(&mut output, *id)?;
                writeln!(output, "event=provider_cancel_resolved outcome={outcome:?}")?;
            }
            RequestTraceEvent::Published {
                id,
                completion,
                tail,
                terminal,
            } => {
                identity(&mut output, *id)?;
                writeln!(
                    output,
                    "event=published tail={tail} terminal={terminal} result={} flags={}",
                    completion.result(),
                    completion.flags()
                )?;
            }
            RequestTraceEvent::HeadReclaimed { ring, head, count } => {
                writeln!(
                    output,
                    "ring={} event=head_reclaimed head={head} count={count}",
                    ring.get()
                )?;
            }
        }
    }
    Ok(output.0)
}
fn identity(output: &mut SnapshotText, id: RequestId) -> fmt::Result {
    write!(
        output,
        "ring={} slot={} generation={} ",
        id.ring().get(),
        id.slot(),
        id.generation()
    )
}
fn simple(output: &mut SnapshotText, id: RequestId, event: &str) -> fmt::Result {
    identity(output, id)?;
    writeln!(output, "event={event}")
}

#[cfg(test)]
mod tests {
    use thekernel_linux_io_uring::RingId;

    use super::*;

    #[test]
    fn executor_stages_preserve_the_reserved_request_identity() {
        use thekernel_linux_io_uring::{RequestDescriptor, RequestOperation, RequestRegistry};

        let mut registry = RequestRegistry::new(RingId::new(7).unwrap(), 1, 1).unwrap();
        let reservation = registry
            .reserve(RequestDescriptor::new(9, RequestOperation::Read))
            .unwrap();
        let id = reservation.id();
        let records = [
            Record {
                sequence: 1,
                nanos: 10,
                event: CaptureEvent::ExecutorStarted(id),
            },
            Record {
                sequence: 2,
                nanos: 20,
                event: CaptureEvent::ExecutorReturned(id),
            },
        ];
        let text = format_snapshot(&records, 0, 0).unwrap();
        for (sequence, nanos, stage) in [(1, 10, "executor_started"), (2, 20, "executor_returned")]
        {
            assert!(text.contains(&alloc::format!(
                "seq={sequence} ns={nanos} ring=7 slot={} generation={} event={stage}\n",
                id.slot(),
                id.generation()
            )));
        }
    }

    #[test]
    fn read_stages_preserve_request_identity_and_fit_target_capture() {
        use thekernel_linux_io_uring::{RequestDescriptor, RequestOperation, RequestRegistry};

        let mut registry = RequestRegistry::new(RingId::new(7).unwrap(), 1, 1).unwrap();
        let request = registry.reserve(RequestDescriptor::new(9, RequestOperation::Read)).unwrap();
        let id = request.id();
        let stages = [ReadStage::PermissionStarted, ReadStage::PermissionReturned,
            ReadStage::ReadStarted, ReadStage::ReadReturned, ReadStage::NotificationReturned];
        for stage in stages {
            let record = Record { sequence: 1, nanos: 10, event: CaptureEvent::ReadStage(id, stage) };
            let text = format_snapshot(&[record], 0, 0).unwrap();
            assert!(text.contains(&alloc::format!(
                "seq=1 ns=10 ring=7 slot={} generation={} event={}\n",
                id.slot(), id.generation(), stage.name(),
            )));
        }
        assert!(64 * (8 + stages.len()) + 2 <= CAPACITY);
    }

    #[test]
    fn capture_is_bounded_and_does_not_overwrite_the_first_failure_context() {
        let mut capture = Capture::new();
        let event = RequestTraceEvent::HeadReclaimed {
            ring: RingId::new(1).unwrap(),
            head: 1,
            count: 1,
        };
        for sequence in 0..CAPACITY as u64 {
            assert!(capture.push(Record {
                sequence,
                nanos: 0,
                event: CaptureEvent::Lifecycle(event)
            }));
        }
        assert!(!capture.push(Record {
            sequence: CAPACITY as u64,
            nanos: 0,
            event: CaptureEvent::Lifecycle(event)
        }));
        assert_eq!(capture.len, CAPACITY);
        assert_eq!(capture.records[0].unwrap().sequence, 0);
        assert_eq!(
            capture.records[CAPACITY - 1].unwrap().sequence,
            CAPACITY as u64 - 1
        );
    }

    #[test]
    fn disabled_producers_do_not_lock_and_contention_is_counted_without_waiting() {
        set_enabled(false);
        clear();
        let event = RequestTraceEvent::HeadReclaimed {
            ring: RingId::new(1).unwrap(),
            head: 1,
            count: 1,
        };
        {
            let _capture = CAPTURE.lock();
            let before = dropped();
            record(event);
            assert_eq!(dropped(), before);
        }
        set_enabled(true);
        {
            let _capture = CAPTURE.lock();
            let before = dropped();
            record(event);
            assert_eq!(dropped(), before + 1);
        }
        set_enabled(false);
        clear();
        assert!(snapshot().unwrap().contains(" dropped=0 "));
    }
}
