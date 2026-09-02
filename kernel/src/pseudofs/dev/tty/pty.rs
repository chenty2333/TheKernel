use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet};
use kspin::{SpinNoIrq, SpinNoPreempt};
use ringbuf::{
    Cons, HeapRb, Prod,
    traits::{Consumer, Observer, Producer},
};

use super::{
    Tty,
    terminal::{
        Terminal,
        ldisc::{
            ExternalRegister, ExternalRegistration, ProcessMode, TtyConfig, TtyRead, TtyWrite,
        },
    },
};

const PTY_BUF_SIZE: usize = 4096;

pub type PtyDriver = Tty<PtyReader, PtyWriter>;

type Buffer = Arc<HeapRb<u8>>;

#[derive(Default)]
struct PtyPairState {
    master_open: bool,
    master_closed: bool,
    slave_open_count: usize,
    slave_ever_opened: bool,
}

struct PtyLifecycle {
    state: SpinNoIrq<PtyPairState>,
    master_waiters: PollSet,
    slave_waiters: PollSet,
}

impl PtyLifecycle {
    const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(PtyPairState {
                master_open: false,
                master_closed: false,
                slave_open_count: 0,
                slave_ever_opened: false,
            }),
            master_waiters: PollSet::new(),
            slave_waiters: PollSet::new(),
        }
    }

    fn wake_all(&self) {
        self.master_waiters.wake();
        self.slave_waiters.wake();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PtySide {
    Master,
    Slave,
}

#[derive(Clone)]
pub(super) struct PtyEndpoint {
    lifecycle: Arc<PtyLifecycle>,
    side: PtySide,
}

impl PtyEndpoint {
    fn new(lifecycle: Arc<PtyLifecycle>, side: PtySide) -> Self {
        Self { lifecycle, side }
    }

    pub(super) fn is_master(&self) -> bool {
        self.side == PtySide::Master
    }

    pub(super) fn open(&self) -> AxResult<()> {
        let result = {
            let mut state = self.lifecycle.state.lock();
            match self.side {
                PtySide::Master if state.master_closed || state.master_open => Err(AxError::Io),
                PtySide::Master => {
                    state.master_open = true;
                    Ok(())
                }
                PtySide::Slave if !state.master_open || state.master_closed => Err(AxError::Io),
                PtySide::Slave => {
                    let Some(next) = state.slave_open_count.checked_add(1) else {
                        return Err(AxError::TooManyOpenFiles);
                    };
                    state.slave_open_count = next;
                    state.slave_ever_opened = true;
                    Ok(())
                }
            }
        };
        if result.is_ok() {
            self.lifecycle.wake_all();
        }
        result
    }

    /// Closes one open-file-description reference.
    ///
    /// Returns `true` only for the permanent master close which also owns the
    /// devpts name lease.
    pub(super) fn close(&self) -> bool {
        let master_final = {
            let mut state = self.lifecycle.state.lock();
            match self.side {
                PtySide::Master if state.master_open => {
                    state.master_open = false;
                    state.master_closed = true;
                    true
                }
                PtySide::Master => false,
                PtySide::Slave if state.slave_open_count != 0 => {
                    state.slave_open_count -= 1;
                    false
                }
                PtySide::Slave => false,
            }
        };
        self.lifecycle.wake_all();
        master_final
    }

    pub(super) fn read_hangup(&self) -> bool {
        let state = self.lifecycle.state.lock();
        match self.side {
            PtySide::Master => state.slave_ever_opened && state.slave_open_count == 0,
            PtySide::Slave => state.master_closed,
        }
    }

    pub(super) fn write_error(&self) -> bool {
        self.side == PtySide::Slave && self.lifecycle.state.lock().master_closed
    }

    pub(super) fn hangup_events(&self) -> IoEvents {
        if !self.read_hangup() {
            return IoEvents::empty();
        }
        match self.side {
            // Linux reports readable plus HUP on a master whose last slave
            // closed; an empty read then reports EIO.
            PtySide::Master => IoEvents::READABLE | IoEvents::HANGUP,
            // A vhangup on the slave produces EOF/EIO while poll exposes the
            // exceptional condition. Linux also reports OUT in this state.
            PtySide::Slave => {
                IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR | IoEvents::HANGUP
            }
        }
    }

    pub(super) fn poll_source(&self) -> &PollSet {
        match self.side {
            PtySide::Master => &self.lifecycle.master_waiters,
            PtySide::Slave => &self.lifecycle.slave_waiters,
        }
    }

    pub(super) fn wake_waiters(&self) {
        self.lifecycle.wake_all();
    }
}

struct ChannelEvents {
    rx: Arc<PollSet>,
    tx: Arc<PollSet>,
}

/// Accounting belongs to the channel, rather than either endpoint.  `pending`
/// is changed only after a byte becomes visible to the peer (or before a byte
/// is returned to it), so it is suitable for `TIOCOUTQ` and drain waits.
struct ChannelState {
    pending: AtomicUsize,
    /// Number of oldest queued bytes which a flush has invalidated.  A reader
    /// discards this prefix before exposing later writes.
    discard: AtomicUsize,
    stopped: AtomicBool,
    gate: SpinNoIrq<()>,
}

impl ChannelState {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            discard: AtomicUsize::new(0),
            stopped: AtomicBool::new(false),
            gate: SpinNoIrq::new(()),
        }
    }
}

impl ChannelEvents {
    fn try_new() -> AxResult<Self> {
        Ok(Self {
            rx: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
            tx: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
        })
    }

    fn wake_all(&self) {
        self.rx.wake();
        self.tx.wake();
    }
}

pub struct PtyReader {
    consumer: Cons<Buffer>,
    events: Arc<ChannelEvents>,
    state: Arc<ChannelState>,
    endpoint: PtyEndpoint,
}

impl TtyRead for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        let _gate = self.state.gate.lock();
        // A flush invalidates a FIFO prefix.  Keeping the exact count avoids
        // dropping writes which race after the flush transaction.
        let mut scratch = [0u8; 80];
        while self.state.discard.load(Ordering::Acquire) != 0 {
            let wanted = self
                .state
                .discard
                .load(Ordering::Acquire)
                .min(scratch.len());
            let dropped = self.consumer.pop_slice(&mut scratch[..wanted]);
            if dropped == 0 {
                break;
            }
            self.state.discard.fetch_sub(dropped, Ordering::AcqRel);
        }
        let read = self.consumer.pop_slice(buf);
        if read != 0 {
            self.state.pending.fetch_sub(read, Ordering::AcqRel);
            // Consuming input creates write capacity. Wake after the ring
            // operation so no waker runs inside ring-buffer critical state.
            self.events.tx.wake();
        }
        Ok(read)
    }

    fn input_eof(&self) -> bool {
        self.endpoint.read_hangup() && self.consumer.is_empty()
    }

    fn flush_input(&mut self) {
        let _gate = self.state.gate.lock();
        let pending = self.state.pending.swap(0, Ordering::AcqRel);
        self.state.discard.fetch_add(pending, Ordering::AcqRel);
        self.events.wake_all();
    }
}

struct PtyWriterInner {
    producer: SpinNoPreempt<Prod<Buffer>>,
    events: Arc<ChannelEvents>,
    state: Arc<ChannelState>,
    endpoint: PtyEndpoint,
}

#[derive(Clone)]
pub struct PtyWriter(Arc<PtyWriterInner>);

impl PtyWriter {
    fn try_new(
        buffer: Buffer,
        events: Arc<ChannelEvents>,
        state: Arc<ChannelState>,
        endpoint: PtyEndpoint,
    ) -> AxResult<Self> {
        Arc::try_new(PtyWriterInner {
            producer: SpinNoPreempt::new(Prod::new(buffer)),
            events,
            state,
            endpoint,
        })
        .map(Self)
        .map_err(|_| AxError::NoMemory)
    }
}

impl TtyWrite for PtyWriter {
    fn write(&self, buf: &[u8]) -> AxResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.0.endpoint.write_error() {
            return Err(AxError::Io);
        }
        if self.0.state.stopped.load(Ordering::Acquire) {
            return Err(AxError::WouldBlock);
        }
        let written = {
            let _gate = self.0.state.gate.lock();
            if self.0.state.stopped.load(Ordering::Acquire) {
                return Err(AxError::WouldBlock);
            }
            let mut producer = self.0.producer.lock();
            let written = producer.push_slice(buf);
            self.0.state.pending.fetch_add(written, Ordering::AcqRel);
            written
        };
        if written == 0 {
            return Err(AxError::WouldBlock);
        }
        self.0.events.rx.wake();
        Ok(written)
    }

    fn poll_write(&self) -> bool {
        !self.0.state.stopped.load(Ordering::Acquire) && !self.0.producer.lock().is_full()
    }

    fn tx_poll_source(&self) -> Option<&Arc<PollSet>> {
        Some(&self.0.events.tx)
    }

    fn wake_waiters(&self) {
        self.0.events.wake_all();
    }

    fn output_pending(&self) -> usize {
        self.0.state.pending.load(Ordering::Acquire)
    }

    fn output_poll_source(&self) -> Option<&Arc<PollSet>> {
        Some(&self.0.events.tx)
    }

    fn flush_output(&self) {
        let _gate = self.0.state.gate.lock();
        let pending = self.0.state.pending.swap(0, Ordering::AcqRel);
        self.0.state.discard.fetch_add(pending, Ordering::AcqRel);
        self.0.events.wake_all();
    }

    fn set_output_stopped(&self, stopped: bool) {
        self.0.state.stopped.store(stopped, Ordering::Release);
        self.0.events.wake_all();
    }
}

fn try_channel(
    reader_endpoint: PtyEndpoint,
    writer_endpoint: PtyEndpoint,
) -> AxResult<(PtyReader, PtyWriter, Arc<ChannelEvents>)> {
    let buffer = Arc::try_new(HeapRb::try_new(PTY_BUF_SIZE).map_err(|_| AxError::NoMemory)?)
        .map_err(|_| AxError::NoMemory)?;
    let events = Arc::try_new(ChannelEvents::try_new()?).map_err(|_| AxError::NoMemory)?;
    let state = Arc::try_new(ChannelState::new()).map_err(|_| AxError::NoMemory)?;
    let reader = PtyReader {
        consumer: Cons::new(buffer.clone()),
        events: events.clone(),
        state: state.clone(),
        endpoint: reader_endpoint,
    };
    let writer = PtyWriter::try_new(buffer, events.clone(), state, writer_endpoint)?;
    Ok((reader, writer, events))
}

fn create_pty_pair_with_external_reader(
    external_reader: bool,
) -> AxResult<(Arc<PtyDriver>, Arc<PtyDriver>)> {
    let lifecycle = Arc::try_new(PtyLifecycle::new()).map_err(|_| AxError::NoMemory)?;
    let master_endpoint = PtyEndpoint::new(lifecycle.clone(), PtySide::Master);
    let slave_endpoint = PtyEndpoint::new(lifecycle, PtySide::Slave);

    let (master_to_slave_reader, master_to_slave_writer, master_to_slave_events) =
        try_channel(slave_endpoint.clone(), master_endpoint.clone())?;
    let (slave_to_master_reader, slave_to_master_writer, slave_to_master_events) =
        try_channel(master_endpoint.clone(), slave_endpoint.clone())?;

    let terminal = Arc::try_new(Terminal::default()).map_err(|_| AxError::NoMemory)?;

    let master = Tty::try_new(
        terminal.clone(),
        TtyConfig {
            reader: slave_to_master_reader,
            writer: master_to_slave_writer,
            process_mode: ProcessMode::None(slave_to_master_events.rx.clone()),
        },
        Some(master_endpoint),
    )?;

    let process_mode = if external_reader {
        let register_source = Arc::clone(&master_to_slave_events.rx);
        let register: ExternalRegister = Box::try_new(move |waker: &Waker| {
            ExternalRegistration::poll(Arc::clone(&register_source), waker)
        })
        .map_err(|_| AxError::NoMemory)?;
        ProcessMode::External(register)
    } else {
        ProcessMode::Manual
    };
    let slave = Tty::try_new(
        terminal,
        TtyConfig {
            reader: master_to_slave_reader,
            writer: slave_to_master_writer,
            process_mode,
        },
        Some(slave_endpoint),
    )?;

    Ok((master, slave))
}

pub(crate) fn create_pty_pair() -> AxResult<(Arc<PtyDriver>, Arc<PtyDriver>)> {
    create_pty_pair_with_external_reader(true)
}

#[cfg(test)]
pub(super) fn create_pty_pair_for_test() -> AxResult<(Arc<PtyDriver>, Arc<PtyDriver>)> {
    create_pty_pair_with_external_reader(false)
}

#[cfg(test)]
mod tests {
    use alloc::{sync::Arc, task::Wake, vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };

    use super::*;

    struct CountWake(AtomicUsize);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn channel() -> (PtyEndpoint, PtyEndpoint, PtyReader, PtyWriter) {
        let lifecycle = Arc::try_new(PtyLifecycle::new()).unwrap();
        let master = PtyEndpoint::new(lifecycle.clone(), PtySide::Master);
        let slave = PtyEndpoint::new(lifecycle, PtySide::Slave);
        master.open().unwrap();
        slave.open().unwrap();
        let (reader, writer, _) = try_channel(slave.clone(), master.clone()).unwrap();
        (master, slave, reader, writer)
    }

    #[test]
    fn full_channel_returns_short_write_then_eagain_and_recovers() {
        let (_master, _slave, mut reader, writer) = channel();
        let input = vec![0x5a; PTY_BUF_SIZE + 37];

        assert_eq!(writer.write(&input), Ok(PTY_BUF_SIZE));
        assert_eq!(
            writer.write(&input[PTY_BUF_SIZE..]),
            Err(AxError::WouldBlock)
        );
        assert!(!writer.poll_write());

        let mut consumed = [0; 113];
        assert_eq!(reader.read(&mut consumed), Ok(consumed.len()));
        assert!(writer.poll_write());
        assert_eq!(writer.write(&input[PTY_BUF_SIZE..]), Ok(37));
        assert!(consumed.iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn consuming_channel_wakes_blocked_writer() {
        let (_master, _slave, mut reader, writer) = channel();
        assert_eq!(writer.write(&vec![1; PTY_BUF_SIZE]), Ok(PTY_BUF_SIZE));

        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let _token = writer.register_tx_waker(&waker).unwrap();
        let mut byte = [0];
        assert_eq!(reader.read(&mut byte), Ok(1));
        assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn lifecycle_matches_master_and_slave_hangup_rules() {
        let (master, slave, mut reader, writer) = channel();
        let (_master_reader, slave_writer, _) = try_channel(master.clone(), slave.clone()).unwrap();
        assert!(master.hangup_events().is_empty());
        assert!(slave.hangup_events().is_empty());

        assert!(!slave.close());
        assert_eq!(
            master.hangup_events().bits(),
            (IoEvents::READABLE | IoEvents::HANGUP).bits()
        );
        // A Unix98 master may buffer output while no slave is open.
        assert_eq!(writer.write(b"pending"), Ok(7));

        slave.open().unwrap();
        assert!(master.hangup_events().is_empty());
        assert!(master.close());
        assert_eq!(
            slave.hangup_events().bits(),
            (IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR | IoEvents::HANGUP).bits()
        );
        let mut pending = [0; 8];
        assert!(!reader.input_eof());
        assert_eq!(reader.read(&mut pending), Ok(7));
        assert_eq!(&pending[..7], b"pending");
        assert!(reader.input_eof());
        assert_eq!(reader.read(&mut pending), Ok(0));
        assert!(slave.write_error());
        assert_eq!(slave_writer.write(b"after-hangup"), Err(AxError::Io));
        assert_eq!(slave_writer.write(b""), Ok(0));
    }

    #[test]
    fn interleaved_producer_consumer_never_loses_accepted_bytes() {
        let (_master, _slave, mut reader, writer) = channel();
        let source = vec![0xa5; PTY_BUF_SIZE * 3 + 29];
        let mut accepted = 0;
        let mut received = 0;
        let mut scratch = [0; 257];

        while received != source.len() {
            if accepted != source.len() {
                match writer.write(&source[accepted..]) {
                    Ok(written) => accepted += written,
                    Err(AxError::WouldBlock) => {}
                    other => panic!("unexpected write result: {other:?}"),
                }
            }
            let read = reader.read(&mut scratch).unwrap();
            assert!(scratch[..read].iter().all(|byte| *byte == 0xa5));
            received += read;
        }
        assert_eq!(accepted, source.len());
    }
}
