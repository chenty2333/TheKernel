use alloc::{boxed::Box, collections::vec_deque::VecDeque, string::String, sync::Arc, vec::Vec};
use core::{
    future::poll_fn,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    task::{Context, Poll, Waker},
};

use axerrno::{AxError, AxResult};
use axpoll::{PollSet, RegisterError, RegistrationToken, UpdateError};
use axtask::{
    AxTaskRef,
    future::{
        IrqWakerRegisterError, IrqWakerToken, IrqWakerUpdateError, block_on, cancel_irq_waker,
        register_irq_waker, update_irq_waker,
    },
};
use linux_raw_sys::general::{
    ECHOCTL, ECHOE, ECHOK, ICRNL, IGNCR, ISIG, ONLCR, OPOST, VEOF, VERASE, VKILL,
};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer, Observer, Producer, Split},
};
use thekernel_linux_signal::SignalInfo;

use super::{Terminal, termios::Termios2};
use crate::task::send_signal_to_process_group;

const BUF_SIZE: usize = 80;
const CANONICAL_BUF_SIZE: usize = 4096;
const CANONICAL_LINE_MAX: usize = CANONICAL_BUF_SIZE - 1;
const ECHO_BUF_SIZE: usize = 4096;
const EXTERNAL_PROGRESS_BUDGET: usize = 64;

type ReadBuf = Arc<ringbuf::StaticRb<u8, BUF_SIZE>>;
pub type ExternalRegister = Box<
    dyn for<'a> Fn(&'a Waker) -> Result<ExternalRegistration, ExternalRegisterError> + Send + Sync,
>;

#[derive(Debug, Clone, Copy)]
pub enum ExternalRegisterError {
    Poll(RegisterError),
    Irq(IrqWakerRegisterError),
}

enum ExternalRegistrationKind {
    Poll {
        source: Arc<PollSet>,
        token: RegistrationToken,
    },
    Irq(IrqWakerToken),
}

pub struct ExternalRegistration {
    kind: Option<ExternalRegistrationKind>,
}

impl ExternalRegistration {
    pub fn poll(source: Arc<PollSet>, waker: &Waker) -> Result<Self, ExternalRegisterError> {
        let token = source
            .register(waker)
            .map_err(ExternalRegisterError::Poll)?;
        Ok(Self {
            kind: Some(ExternalRegistrationKind::Poll { source, token }),
        })
    }

    pub fn irq(irq: usize, waker: &Waker) -> Result<Self, ExternalRegisterError> {
        let token = register_irq_waker(irq, waker).map_err(ExternalRegisterError::Irq)?;
        Ok(Self {
            kind: Some(ExternalRegistrationKind::Irq(token)),
        })
    }

    fn update(&mut self, waker: &Waker) -> Result<(), ()> {
        match self.kind.as_ref().ok_or(())? {
            ExternalRegistrationKind::Poll { source, token } => {
                source.update(*token, waker).map_err(|_| ())
            }
            ExternalRegistrationKind::Irq(token) => {
                update_irq_waker(*token, waker).map_err(|_error: IrqWakerUpdateError| ())
            }
        }
    }

    fn cancel(&mut self) {
        if let Some(kind) = self.kind.take() {
            match kind {
                ExternalRegistrationKind::Poll { source, token } => {
                    source.cancel(token);
                }
                ExternalRegistrationKind::Irq(token) => {
                    cancel_irq_waker(token);
                }
            }
        }
    }
}

impl Drop for ExternalRegistration {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// How should we process inputs?
pub enum ProcessMode {
    /// Process inputs only on call to `read`.
    Manual,
    /// Spawn a fallible, cancellable task and use the callback to subscribe to
    /// the external source which feeds the reader.
    External(ExternalRegister),
    /// Do not apply the slave line discipline. This is the PTY master side;
    /// the argument is the source-data wait set.
    None(Arc<PollSet>),
}

pub struct TtyConfig<R, W> {
    pub reader: R,
    pub writer: W,
    pub process_mode: ProcessMode,
}

pub trait TtyRead: Send + Sync + 'static {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize>;

    /// Returns true only after the producer has permanently closed and every
    /// byte accepted by the underlying transport has been consumed.
    fn input_eof(&self) -> bool {
        false
    }

    /// Discard transport bytes which have not entered the line discipline.
    /// Implementations which have no buffered transport may leave this empty.
    fn flush_input(&mut self) {}
}

pub trait TtyWrite: Send + Sync + 'static {
    fn write(&self, buf: &[u8]) -> AxResult<usize>;

    fn poll_write(&self) -> bool {
        true
    }

    /// Stable bounded source used when writes transition from blocked to ready.
    fn tx_poll_source(&self) -> Option<&Arc<PollSet>> {
        None
    }

    fn register_tx_waker(&self, waker: &Waker) -> Result<Option<RegistrationToken>, RegisterError> {
        if let Some(source) = self.tx_poll_source() {
            source.register(waker).map(Some)
        } else {
            waker.wake_by_ref();
            Ok(None)
        }
    }

    fn update_tx_waker(&self, token: RegistrationToken, waker: &Waker) -> Result<(), UpdateError> {
        self.tx_poll_source()
            .ok_or(UpdateError::InvalidToken)?
            .update(token, waker)
    }

    fn cancel_tx_waker(&self, token: RegistrationToken) -> bool {
        self.tx_poll_source()
            .is_some_and(|source| source.cancel(token))
    }

    fn wake_waiters(&self) {}

    /// Exact number of bytes accepted by this transport but not yet consumed
    /// by its peer.  This is the `TIOCOUTQ` definition, not a readiness hint.
    fn output_pending(&self) -> usize {
        0
    }

    /// Stable completion source for an output drain or flow-control change.
    fn output_poll_source(&self) -> Option<&Arc<PollSet>> {
        self.tx_poll_source()
    }

    /// Drop accepted output which has not reached the peer.
    fn flush_output(&self) {}

    /// Apply local software output flow control. A stopped writer must apply
    /// backpressure until it is resumed, rather than pretending output drained.
    fn set_output_stopped(&self, _stopped: bool) {}
}

struct InputReader<R, W> {
    terminal: Arc<Terminal>,
    reader: R,
    writer: W,
    buf_tx: CachingProd<ReadBuf>,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,
    // Linux N_TTY bounds a canonical input line to 4096 bytes. Reserving the
    // entire limit during admission means input processing never allocates.
    line_buf: Vec<u8>,
    line_read: Option<usize>,
    echo_buf: VecDeque<u8>,
    empty_eof_pending: Arc<AtomicBool>,
    source_drained: Arc<AtomicBool>,
    echo_pending: Arc<core::sync::atomic::AtomicUsize>,
}

impl<R: TtyRead, W: TtyWrite> InputReader<R, W> {
    fn flush_input(&mut self) {
        self.reader.flush_input();
        self.read_range = 0..0;
        self.line_buf.clear();
        self.line_read = None;
        self.echo_buf.clear();
        self.echo_pending.store(0, Ordering::Release);
        self.empty_eof_pending.store(false, Ordering::Release);
        self.source_drained.store(false, Ordering::Release);
    }

    fn poll(&mut self) -> AxResult<bool> {
        let mut progressed = self.flush_echo();
        // An empty canonical VEOF is a real zero-length record. Do not consume
        // later transport bytes until userspace has observed that record.
        if self.empty_eof_pending.load(Ordering::Acquire) {
            return Ok(progressed);
        }
        if self.read_range.is_empty() {
            let read = self.reader.read(&mut self.read_buf)?;
            self.read_range = 0..read;
            progressed |= read != 0;
        }
        let term = self.terminal.load_termios();
        if term.canonical()
            && self.reader.input_eof()
            && self.read_range.is_empty()
            && self.line_read.is_none()
            && !self.line_buf.is_empty()
        {
            // A hangup terminates a partial canonical record. Move it through
            // the same bounded publication path before reporting EOF.
            self.line_read = Some(0);
            progressed = true;
        }
        loop {
            if let Some(offset) = &mut self.line_read {
                let read = self.buf_tx.push_slice(&self.line_buf[*offset..]);
                if read == 0 {
                    break;
                }
                progressed = true;
                *offset += read;
                if *offset == self.line_buf.len() {
                    self.line_read = None;
                    self.line_buf.clear();
                }
                continue;
            }
            if self.buf_tx.is_full() || self.read_range.is_empty() {
                break;
            }
            let progress_before_char = progressed;
            let mut ch = self.read_buf[self.read_range.start];
            self.read_range.start += 1;
            progressed = true;

            if ch == b'\r' {
                if term.has_iflag(IGNCR) {
                    continue;
                }
                if term.has_iflag(ICRNL) {
                    ch = b'\n';
                }
            }

            self.check_send_signal(&term, ch);

            if term.echo() {
                self.output_char(&term, ch);
            }
            if !term.canonical() {
                let _ = self.buf_tx.try_push(ch);
                continue;
            }

            if term.matches_special_char(VKILL, ch) {
                self.line_buf.clear();
                if term.has_lflag(ECHOK) && term.echo() {
                    self.queue_echo(b"\n");
                }
                continue;
            }
            if term.matches_special_char(VERASE, ch) {
                self.line_buf.pop();
                continue;
            }

            let is_veof = term.matches_special_char(VEOF, ch);
            if term.is_eol(ch) || is_veof {
                if !is_veof && self.line_buf.len() < CANONICAL_BUF_SIZE {
                    self.line_buf.push(ch);
                }
                if self.line_buf.is_empty() && is_veof {
                    // Preserve record order: do not publish an empty record in
                    // front of bytes from an earlier completed line.
                    if !self.buf_tx.is_empty() {
                        self.read_range.start -= 1;
                        progressed = progress_before_char;
                        break;
                    }
                    self.empty_eof_pending.store(true, Ordering::Release);
                    break;
                } else if !self.line_buf.is_empty() {
                    self.line_read = Some(0);
                }
                continue;
            }

            // Match N_TTY's bounded overflow behavior: keep accepting control
            // processing but do not grow a canonical line beyond 4095 bytes,
            // leaving one slot for its delimiter.
            if (ch == b' ' || ch.is_ascii_graphic()) && self.line_buf.len() < CANONICAL_LINE_MAX {
                self.line_buf.push(ch);
            }
        }

        // A PTY hangup is visible only after raw transport data, the staging
        // buffer, and a partial canonical line have all advanced into the
        // public line-discipline ring. Userspace consumption of that public
        // ring is checked separately by `input_drained()`.
        let drained = self.reader.input_eof()
            && self.read_range.is_empty()
            && self.line_read.is_none()
            && self.line_buf.is_empty();
        let was_drained = self.source_drained.swap(drained, Ordering::AcqRel);
        progressed |= drained != was_drained;
        Ok(progressed)
    }

    fn check_send_signal(&self, term: &Termios2, ch: u8) {
        // The current signal path implements canonical N_TTY delivery. A
        // noncanonical+ISIG configuration is rejected by termios admission
        // until its flush and byte-consumption rules are implemented.
        if !term.canonical() || !term.has_lflag(ISIG) {
            return;
        }
        if let Some(signo) = term.signo_for(ch)
            && let Some(pg) = self.terminal.job_control.foreground()
        {
            let sig = SignalInfo::new_kernel(signo);
            if let Err(err) = send_signal_to_process_group(pg.pgid(), Some(sig)) {
                warn!("Failed to send signal: {err:?}");
            }
        }
    }

    fn output_char(&mut self, term: &Termios2, ch: u8) {
        match ch {
            b'\n' => self.queue_echo(b"\n"),
            b'\r' => self.queue_echo(b"\r\n"),
            ch if term.canonical()
                && term.matches_special_char(VERASE, ch)
                && term.has_lflag(ECHOE) =>
            {
                self.queue_echo(b"\x08 \x08")
            }
            ch if term.canonical()
                && term.matches_special_char(VERASE, ch)
                && term.has_lflag(ECHOCTL) =>
            {
                self.queue_echo(b"^?")
            }
            ch if term.canonical() && term.matches_special_char(VERASE, ch) => {}
            ch if ch == b' ' || ch.is_ascii_graphic() => self.queue_echo(&[ch]),
            ch if ch.is_ascii_control() && term.has_lflag(ECHOCTL) => {
                self.queue_echo(&[b'^', ch + 0x40]);
            }
            other => {
                warn!("Ignored echo char: {other:#x}");
            }
        }
    }

    fn flush_echo(&mut self) -> bool {
        let mut progressed = false;
        loop {
            let (first, second) = self.echo_buf.as_slices();
            let pending = if !first.is_empty() { first } else { second };
            if pending.is_empty() {
                break;
            }
            let written = match self.writer.write(pending) {
                Ok(0) | Err(AxError::WouldBlock) => break,
                Ok(written) => written.min(pending.len()),
                Err(_) => {
                    self.echo_buf.clear();
                    self.echo_pending.store(0, Ordering::Release);
                    break;
                }
            };
            for _ in 0..written {
                self.echo_buf.pop_front();
            }
            self.echo_pending.fetch_sub(written, Ordering::AcqRel);
            progressed = true;
        }
        progressed
    }

    fn queue_echo(&mut self, bytes: &[u8]) {
        self.flush_echo();
        let written = if self.echo_buf.is_empty() {
            match self.writer.write(bytes) {
                Ok(written) => written.min(bytes.len()),
                Err(AxError::WouldBlock) => 0,
                Err(_) => return,
            }
        } else {
            0
        };
        for byte in &bytes[written..] {
            if self.echo_buf.len() == ECHO_BUF_SIZE {
                // Linux N_TTY also bounds and eventually discards echo state
                // under sustained backpressure. Never grow or allocate here.
                break;
            }
            self.echo_buf.push_back(*byte);
            self.echo_pending.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn echo_waiting(&self) -> bool {
        !self.echo_buf.is_empty()
    }
}

struct SimpleReader<R> {
    terminal: Arc<Terminal>,
    reader: R,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,
    pending: Option<u8>,
    buf_tx: CachingProd<ReadBuf>,
}

impl<R: TtyRead> SimpleReader<R> {
    fn poll(&mut self) -> AxResult<()> {
        if self.read_range.is_empty() {
            let read = self.reader.read(&mut self.read_buf)?;
            self.read_range = 0..read;
        }

        if let Some(pending) = self.pending.take()
            && self.buf_tx.try_push(pending).is_err()
        {
            self.pending = Some(pending);
            return Ok(());
        }

        let term = self.terminal.load_termios();
        let map_newline = term.has_oflag(OPOST) && term.has_oflag(ONLCR);
        while !self.buf_tx.is_full() && !self.read_range.is_empty() {
            let ch = self.read_buf[self.read_range.start];
            self.read_range.start += 1;
            if ch == b'\n' && map_newline {
                // Preserve both bytes of the master-side CRLF expansion even
                // when only one line-discipline slot remains.
                if self.buf_tx.try_push(b'\r').is_err() {
                    self.read_range.start -= 1;
                    break;
                }
                if self.buf_tx.try_push(b'\n').is_err() {
                    self.pending = Some(b'\n');
                    break;
                }
            } else if self.buf_tx.try_push(ch).is_err() {
                self.read_range.start -= 1;
                break;
            }
        }
        Ok(())
    }
}

struct WorkerControl {
    cancelled: AtomicBool,
    terminated: AtomicBool,
    failure: AtomicU8,
    flush_requested: AtomicBool,
    flush_generation: AtomicU32,
    flush_complete: AtomicU32,
    wake: Arc<PollSet>,
}

impl WorkerControl {
    fn try_new() -> AxResult<Self> {
        Ok(Self {
            cancelled: AtomicBool::new(false),
            terminated: AtomicBool::new(false),
            failure: AtomicU8::new(0),
            flush_requested: AtomicBool::new(false),
            flush_generation: AtomicU32::new(0),
            flush_complete: AtomicU32::new(0),
            wake: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
        })
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.wake.wake();
    }

    fn request_flush(&self) -> u32 {
        let generation = self
            .flush_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.flush_requested.store(true, Ordering::Release);
        self.wake.wake();
        generation
    }

    fn record_failure(&self, error: AxError) {
        let code = match error {
            AxError::NoMemory => 1,
            AxError::OutOfRange => 2,
            AxError::InvalidInput => 3,
            _ => 4,
        };
        let _ = self
            .failure
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
    }

    fn failure(&self) -> Option<AxError> {
        match self.failure.load(Ordering::Acquire) {
            0 => None,
            1 => Some(AxError::NoMemory),
            2 => Some(AxError::OutOfRange),
            3 => Some(AxError::InvalidInput),
            _ => Some(AxError::BadState),
        }
    }
}

struct OwnedPollSource {
    source: Arc<PollSet>,
    token: Option<RegistrationToken>,
}

impl OwnedPollSource {
    fn register(source: Arc<PollSet>, waker: &Waker) -> Result<Self, RegisterError> {
        let token = source.register(waker)?;
        Ok(Self {
            source,
            token: Some(token),
        })
    }

    fn update(&self, waker: &Waker) -> Result<(), UpdateError> {
        self.source
            .update(self.token.ok_or(UpdateError::InvalidToken)?, waker)
    }

    fn cancel(&mut self) {
        if let Some(token) = self.token.take() {
            self.source.cancel(token);
        }
    }
}

impl Drop for OwnedPollSource {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct WorkerRegistrations {
    capacity: OwnedPollSource,
    external: ExternalRegistration,
    control: OwnedPollSource,
    echo: Option<OwnedPollSource>,
}

impl WorkerRegistrations {
    fn register<R: TtyRead, W: TtyWrite>(
        reader: &InputReader<R, W>,
        capacity: &Arc<PollSet>,
        control: &Arc<WorkerControl>,
        register: &ExternalRegister,
        waker: &Waker,
    ) -> Result<Self, AxError> {
        let capacity = OwnedPollSource::register(Arc::clone(capacity), waker)
            .map_err(map_poll_register_error)?;
        let external = register(waker).map_err(map_external_register_error)?;
        let control = OwnedPollSource::register(Arc::clone(&control.wake), waker)
            .map_err(map_poll_register_error)?;
        let echo = if reader.echo_waiting() {
            reader
                .writer
                .tx_poll_source()
                .map(|source| OwnedPollSource::register(Arc::clone(source), waker))
                .transpose()
                .map_err(map_poll_register_error)?
        } else {
            None
        };
        Ok(Self {
            capacity,
            external,
            control,
            echo,
        })
    }

    fn update(&mut self, waker: &Waker) -> Result<(), ()> {
        let mut failed = self.capacity.update(waker).is_err();
        failed |= self.external.update(waker).is_err();
        failed |= self.control.update(waker).is_err();
        if let Some(echo) = self.echo.as_ref() {
            failed |= echo.update(waker).is_err();
        }
        if failed { Err(()) } else { Ok(()) }
    }

    fn includes_echo(&self) -> bool {
        self.echo.is_some()
    }
}

fn map_poll_register_error(error: RegisterError) -> AxError {
    match error {
        RegisterError::Full => AxError::NoMemory,
        RegisterError::Closed => AxError::BadState,
        RegisterError::TokenSpaceExhausted => AxError::OutOfRange,
    }
}

fn map_external_register_error(error: ExternalRegisterError) -> AxError {
    match error {
        ExternalRegisterError::Poll(error) => map_poll_register_error(error),
        ExternalRegisterError::Irq(error) => match error {
            IrqWakerRegisterError::HookUnavailable => AxError::BadState,
            IrqWakerRegisterError::HookInstallationInProgress
            | IrqWakerRegisterError::SourceCapacityExhausted => AxError::NoMemory,
            IrqWakerRegisterError::Waiter(error) => map_poll_register_error(error),
        },
    }
}

struct WorkerExit(Arc<WorkerControl>);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.terminated.store(true, Ordering::Release);
        self.0.wake.wake();
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ExternalInputAction {
    Stop,
    Yield,
    Wait,
}

fn drive_external_input<R: TtyRead, W: TtyWrite>(
    reader: &mut InputReader<R, W>,
    readable: &Arc<PollSet>,
    control: &Arc<WorkerControl>,
) -> ExternalInputAction {
    if control.flush_requested.swap(false, Ordering::AcqRel) {
        let generation = control.flush_generation.load(Ordering::Acquire);
        reader.flush_input();
        control.flush_complete.store(generation, Ordering::Release);
        readable.wake();
    }
    if control.cancelled.load(Ordering::Acquire) {
        return ExternalInputAction::Stop;
    }

    let mut budget = EXTERNAL_PROGRESS_BUDGET;
    while budget != 0
        && !control.cancelled.load(Ordering::Acquire)
        && matches!(reader.poll(), Ok(true))
    {
        budget -= 1;
        readable.wake();
    }
    if control.cancelled.load(Ordering::Acquire) {
        return ExternalInputAction::Stop;
    }
    if budget == 0 {
        // A continuously replenished source must not turn one operation phase
        // into an unbounded loop or delay final-OFD cancellation indefinitely.
        return ExternalInputAction::Yield;
    }

    ExternalInputAction::Wait
}

fn arm_external_input<R: TtyRead, W: TtyWrite>(
    reader: &InputReader<R, W>,
    capacity: &Arc<PollSet>,
    control: &Arc<WorkerControl>,
    register: &ExternalRegister,
    registration: &mut Option<WorkerRegistrations>,
) -> Result<bool, AxError> {
    if registration.is_none()
        || (reader.echo_waiting()
            && registration
                .as_ref()
                .is_some_and(|registration| !registration.includes_echo()))
    {
        drop(registration.take());
        *registration = Some(WorkerRegistrations::register(
            reader,
            capacity,
            control,
            register,
            Waker::noop(),
        )?);
        return Ok(true);
    }
    Ok(false)
}

fn poll_external_input_wait(
    control: &Arc<WorkerControl>,
    registration: &mut Option<WorkerRegistrations>,
    cx: &mut Context<'_>,
) -> Poll<()> {
    if control.cancelled.load(Ordering::Acquire) {
        return Poll::Ready(());
    }
    if registration
        .as_mut()
        .is_none_or(|registration| registration.update(cx.waker()).is_err())
    {
        return Poll::Ready(());
    }
    Poll::Pending
}

fn run_external_input<R: TtyRead, W: TtyWrite>(
    mut reader: InputReader<R, W>,
    readable: Arc<PollSet>,
    capacity: Arc<PollSet>,
    control: Arc<WorkerControl>,
    register: ExternalRegister,
) -> Result<(), axtask::future::BlockOnError> {
    let _exit = WorkerExit(control.clone());
    let mut registration = None;
    loop {
        match drive_external_input(&mut reader, &readable, &control) {
            ExternalInputAction::Stop => return Ok(()),
            ExternalInputAction::Yield => {
                axtask::yield_now();
                continue;
            }
            ExternalInputAction::Wait => {}
        }

        match arm_external_input(&reader, &capacity, &control, &register, &mut registration) {
            // Complete the check-arm-check sequence outside the block session.
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                control.record_failure(error);
                readable.wake();
                return Ok(());
            }
        }

        block_on(poll_fn(|cx| {
            poll_external_input_wait(&control, &mut registration, cx)
        }))?;
        // Cancellation and destruction of owned registrations belong to the
        // operation phase, after the block session has ended.
        drop(registration.take());
    }
}

struct ExternalProcessor {
    poll_rx: Arc<PollSet>,
    control: Arc<WorkerControl>,
    source_drained: Arc<AtomicBool>,
    echo_pending: Arc<core::sync::atomic::AtomicUsize>,
    task: Option<AxTaskRef>,
}

impl Drop for ExternalProcessor {
    fn drop(&mut self) {
        self.control.cancel();
        if let Some(task) = self.task.take() {
            match task.join() {
                Ok(_) => debug_assert!(self.control.terminated.load(Ordering::Acquire)),
                Err(error) => {
                    self.control.record_failure(error.into());
                    error!("line-discipline worker join failed: {error}");
                }
            }
        }
    }
}

enum Processor<R, W> {
    Manual(InputReader<R, W>),
    External(ExternalProcessor),
    None(SimpleReader<R>, Arc<PollSet>),
}

pub struct LineDiscipline<R, W> {
    terminal: Arc<Terminal>,
    buf_rx: CachingCons<ReadBuf>,
    poll_tx: Arc<PollSet>,
    empty_eof_pending: Arc<AtomicBool>,
    source_drained: Arc<AtomicBool>,
    echo_pending: Arc<core::sync::atomic::AtomicUsize>,
    processor: Processor<R, W>,
}

impl<R: TtyRead, W: TtyWrite> LineDiscipline<R, W> {
    pub fn try_new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> AxResult<Self> {
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, BUF_SIZE>::default())
            .map_err(|_| AxError::NoMemory)?;
        let (buf_tx, buf_rx) = read_buf.split();

        let empty_eof_pending =
            Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let source_drained = Arc::try_new(AtomicBool::new(false)).map_err(|_| AxError::NoMemory)?;
        let echo_pending =
            Arc::try_new(core::sync::atomic::AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let mut line_buf = Vec::new();
        line_buf
            .try_reserve_exact(CANONICAL_BUF_SIZE)
            .map_err(|_| AxError::NoMemory)?;
        let mut echo_buf = VecDeque::new();
        echo_buf
            .try_reserve_exact(ECHO_BUF_SIZE)
            .map_err(|_| AxError::NoMemory)?;
        let reader = InputReader {
            terminal: terminal.clone(),
            reader: config.reader,
            writer: config.writer,
            buf_tx,
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_read: None,
            echo_buf,
            empty_eof_pending: empty_eof_pending.clone(),
            source_drained: source_drained.clone(),
            echo_pending: echo_pending.clone(),
        };

        let poll_tx = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
        let processor = match config.process_mode {
            ProcessMode::Manual => Processor::Manual(reader),
            ProcessMode::External(register) => {
                let poll_rx = Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?;
                let control =
                    Arc::try_new(WorkerControl::try_new()?).map_err(|_| AxError::NoMemory)?;
                let mut name = String::new();
                name.try_reserve_exact("tty-reader".len())
                    .map_err(|_| AxError::NoMemory)?;
                name.push_str("tty-reader");

                let task_poll_rx = poll_rx.clone();
                let task_poll_tx = poll_tx.clone();
                let task_control = control.clone();
                let task = axtask::try_spawn_with_name(
                    move || match run_external_input(
                        reader,
                        task_poll_rx.clone(),
                        task_poll_tx,
                        task_control.clone(),
                        register,
                    ) {
                        Ok(()) => {}
                        Err(error) => {
                            task_control.record_failure(error.into());
                            task_poll_rx.wake();
                        }
                    },
                    name,
                )?;
                Processor::External(ExternalProcessor {
                    poll_rx,
                    control,
                    source_drained: source_drained.clone(),
                    echo_pending: echo_pending.clone(),
                    task: Some(task),
                })
            }
            ProcessMode::None(poll_rx) => Processor::None(
                SimpleReader {
                    terminal: terminal.clone(),
                    reader: reader.reader,
                    read_buf: [0; BUF_SIZE],
                    read_range: 0..0,
                    pending: None,
                    buf_tx: reader.buf_tx,
                },
                poll_rx,
            ),
        };
        Ok(Self {
            terminal,
            buf_rx,
            poll_tx,
            empty_eof_pending,
            source_drained,
            echo_pending,
            processor,
        })
    }

    pub fn readable_len(&mut self) -> usize {
        let _ = self.refill_read_buffer();
        self.buf_rx.occupied_len()
    }

    pub fn output_pending(&self) -> usize {
        self.echo_pending.load(Ordering::Acquire)
    }

    pub fn poll_read(&mut self) -> bool {
        let _ = self.refill_read_buffer();
        !self.buf_rx.is_empty() || self.empty_eof_pending.load(Ordering::Acquire)
    }

    fn refill_read_buffer(&mut self) -> AxResult<()> {
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.poll()?;
            }
            Processor::None(reader, _) => reader.poll()?,
            Processor::External(processor) => {
                if let Some(error) = processor.control.failure() {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub fn readiness_source(&self) -> Option<&Arc<PollSet>> {
        match &self.processor {
            // Injected console input wakes `poll_tx`; manual transports use
            // the same stable source so epoll can sleep until the router
            // delivers bytes to this specific line discipline.
            Processor::Manual(_) => Some(&self.poll_tx),
            Processor::External(processor) => Some(&processor.poll_rx),
            Processor::None(_, set) => Some(set),
        }
    }

    /// Delivers bytes from a single owning transport into a manually-driven
    /// line discipline.  The bounded staging buffer deliberately reports
    /// backpressure rather than stealing bytes into another terminal.
    pub fn inject_input(&mut self, bytes: &[u8]) -> AxResult<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let Processor::Manual(reader) = &mut self.processor else {
            return Err(AxError::BadState);
        };
        if !reader.read_range.is_empty() || bytes.len() > reader.read_buf.len() {
            return Err(AxError::WouldBlock);
        }
        reader.read_buf[..bytes.len()].copy_from_slice(bytes);
        reader.read_range = 0..bytes.len();
        reader.poll()?;
        self.poll_tx.wake();
        Ok(())
    }

    /// Flush every input stage owned by this discipline.  The transport is
    /// asked first so a concurrent refill cannot republish pre-flush bytes.
    pub fn flush_input(&mut self) -> AxResult<()> {
        let external_flush = match &mut self.processor {
            Processor::External(processor) => Some((
                processor.poll_rx.clone(),
                processor.control.clone(),
                processor.control.request_flush(),
            )),
            _ => None,
        };
        match &mut self.processor {
            Processor::Manual(reader) => {
                reader.flush_input();
            }
            Processor::None(reader, _) => {
                reader.reader.flush_input();
                reader.read_range = 0..0;
                reader.pending = None;
            }
            Processor::External(_) => {}
        }
        if let Some((source, control, generation)) = external_flush {
            crate::readiness::block_on_poll_set(&source, || {
                if control.cancelled.load(Ordering::Acquire) {
                    return Err(AxError::Io);
                }
                if control.flush_complete.load(Ordering::Acquire) >= generation {
                    Ok(())
                } else {
                    Err(AxError::WouldBlock)
                }
            })?;
        }
        let mut discarded = [0u8; BUF_SIZE];
        while self.buf_rx.pop_slice(&mut discarded) != 0 {}
        self.empty_eof_pending.store(false, Ordering::Release);
        self.source_drained.store(false, Ordering::Release);
        self.poll_tx.wake();
        Ok(())
    }

    /// Stops input processing and wakes all readers waiting on this discipline.
    pub fn hangup(&mut self) {
        match &self.processor {
            Processor::External(processor) => {
                processor.control.cancel();
                processor.poll_rx.wake();
            }
            Processor::None(_, poll_rx) => {
                poll_rx.wake();
            }
            Processor::Manual(_) => {}
        }
        self.poll_tx.wake();
    }

    pub fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.refill_read_buffer()?;
        if matches!(self.processor, Processor::None(_, _)) {
            let read = self.buf_rx.pop_slice(buf);
            return if read == 0 {
                Err(AxError::WouldBlock)
            } else {
                Ok(read)
            };
        }

        let term = self.terminal.load_termios();
        if term.canonical()
            && self.buf_rx.is_empty()
            && self.empty_eof_pending.swap(false, Ordering::AcqRel)
        {
            self.poll_tx.wake();
            return Ok(0);
        }
        if term.canonical() && self.buf_rx.is_empty() {
            return Err(AxError::WouldBlock);
        }

        let total_read = self.buf_rx.pop_slice(buf);
        self.poll_tx.wake();
        Ok(total_read)
    }

    /// Returns true only after the external source and every bounded input
    /// stage, including the public read ring, have drained.
    pub fn input_drained(&self) -> bool {
        let source_drained = match &self.processor {
            Processor::External(processor) => processor.source_drained.load(Ordering::Acquire),
            _ => self.source_drained.load(Ordering::Acquire),
        };
        source_drained && self.buf_rx.is_empty() && !self.empty_eof_pending.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{sync::Arc, task::Wake, vec, vec::Vec};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Waker,
    };
    use std::{sync::mpsc, thread, time::Duration};

    use kspin::SpinNoIrq;

    use super::*;

    struct VecReader {
        input: Vec<u8>,
        offset: usize,
        eof: bool,
    }

    impl TtyRead for VecReader {
        fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
            let remaining = &self.input[self.offset..];
            let read = remaining.len().min(buf.len());
            buf[..read].copy_from_slice(&remaining[..read]);
            self.offset += read;
            Ok(read)
        }

        fn input_eof(&self) -> bool {
            self.eof && self.offset == self.input.len()
        }
    }

    struct SharedVecReader {
        input: Vec<u8>,
        offset: Arc<AtomicUsize>,
        eof: bool,
    }

    struct EndlessKillReader(Arc<AtomicUsize>);

    impl TtyRead for EndlessKillReader {
        fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
            self.0.fetch_add(1, Ordering::Relaxed);
            // Default VKILL (^U) is consumed by canonical processing without
            // filling the public ring, so this models a perpetually ready
            // source without relying on a concurrent consumer.
            buf.fill(b'U' - 0x40);
            Ok(buf.len())
        }
    }

    impl TtyRead for SharedVecReader {
        fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
            let offset = self.offset.load(Ordering::Acquire);
            let remaining = &self.input[offset..];
            let read = remaining.len().min(buf.len());
            buf[..read].copy_from_slice(&remaining[..read]);
            self.offset.store(offset + read, Ordering::Release);
            Ok(read)
        }

        fn input_eof(&self) -> bool {
            self.eof && self.offset.load(Ordering::Acquire) == self.input.len()
        }
    }

    #[derive(Clone, Copy)]
    struct Sink;

    impl TtyWrite for Sink {
        fn write(&self, buf: &[u8]) -> AxResult<usize> {
            Ok(buf.len())
        }
    }

    struct EchoState {
        budget: usize,
        output: Vec<u8>,
    }

    struct BoundedSink(Arc<SpinNoIrq<EchoState>>);

    impl TtyWrite for BoundedSink {
        fn write(&self, buf: &[u8]) -> AxResult<usize> {
            let mut state = self.0.lock();
            if state.budget == 0 {
                return Err(AxError::WouldBlock);
            }
            let written = state.budget.min(buf.len());
            state.output.extend_from_slice(&buf[..written]);
            state.budget -= written;
            Ok(written)
        }
    }

    struct CountWake(AtomicUsize);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct ThreadWake(thread::Thread);

    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    fn master_side_output(opost: bool, onlcr: bool) -> Vec<u8> {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        terminal
            .termios
            .lock()
            .set_output_processing_for_test(opost, onlcr);
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: vec![b'\n'],
                    offset: 0,
                    eof: true,
                },
                writer: Sink,
                process_mode: ProcessMode::None(Arc::try_new(PollSet::new()).unwrap()),
            },
        )
        .unwrap();
        let mut output = [0; 2];
        let read = ldisc.read(&mut output).unwrap();
        output[..read].to_vec()
    }

    #[test]
    fn master_side_newline_mapping_obeys_opost_and_onlcr() {
        assert_eq!(master_side_output(true, true), b"\r\n");
        assert_eq!(master_side_output(false, true), b"\n");
        assert_eq!(master_side_output(true, false), b"\n");
        assert_eq!(master_side_output(false, false), b"\n");
    }

    #[test]
    fn worker_cancel_sets_state_and_wakes_registered_task() {
        let control = WorkerControl::try_new().unwrap();
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let _token = control.wake.register(&Waker::from(wake.clone())).unwrap();

        control.cancel();

        assert!(control.cancelled.load(Ordering::Acquire));
        assert_eq!(wake.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn external_worker_yields_at_progress_budget_and_then_observes_cancel() {
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, BUF_SIZE>::default()).unwrap();
        let (buf_tx, _buf_rx) = read_buf.split();
        let mut line_buf = Vec::new();
        line_buf.try_reserve_exact(CANONICAL_BUF_SIZE).unwrap();
        let mut echo_buf = VecDeque::new();
        echo_buf.try_reserve_exact(ECHO_BUF_SIZE).unwrap();
        let reads = Arc::try_new(AtomicUsize::new(0)).unwrap();
        let mut reader = InputReader {
            terminal: Arc::try_new(Terminal::default()).unwrap(),
            reader: EndlessKillReader(reads.clone()),
            writer: Sink,
            buf_tx,
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_read: None,
            echo_buf,
            empty_eof_pending: Arc::try_new(AtomicBool::new(false)).unwrap(),
            source_drained: Arc::try_new(AtomicBool::new(false)).unwrap(),
        };
        let readable = Arc::new(PollSet::new());
        let control = Arc::new(WorkerControl::try_new().unwrap());

        assert_eq!(
            drive_external_input(&mut reader, &readable, &control),
            ExternalInputAction::Yield
        );
        assert_eq!(reads.load(Ordering::Relaxed), EXTERNAL_PROGRESS_BUDGET);

        control.cancel();
        assert_eq!(
            drive_external_input(&mut reader, &readable, &control),
            ExternalInputAction::Stop
        );
        assert_eq!(reads.load(Ordering::Relaxed), EXTERNAL_PROGRESS_BUDGET);
    }

    #[test]
    fn external_worker_drains_all_stages_then_cancels_and_joins() {
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, BUF_SIZE>::default()).unwrap();
        let (buf_tx, mut buf_rx) = read_buf.split();
        let mut line_buf = Vec::new();
        line_buf.try_reserve_exact(CANONICAL_BUF_SIZE).unwrap();
        let mut echo_buf = VecDeque::new();
        echo_buf.try_reserve_exact(ECHO_BUF_SIZE).unwrap();
        let consumed = Arc::try_new(AtomicUsize::new(0)).unwrap();
        let source_drained = Arc::try_new(AtomicBool::new(false)).unwrap();
        let input_len = BUF_SIZE * 3 + 1;
        let mut reader = InputReader {
            terminal: Arc::try_new(Terminal::default()).unwrap(),
            reader: SharedVecReader {
                input: vec![b'a'; input_len],
                offset: consumed.clone(),
                eof: true,
            },
            writer: Sink,
            buf_tx,
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_read: None,
            echo_buf,
            empty_eof_pending: Arc::try_new(AtomicBool::new(false)).unwrap(),
            source_drained: source_drained.clone(),
        };

        let readable = Arc::try_new(PollSet::new()).unwrap();
        let poll_tx = Arc::try_new(PollSet::new()).unwrap();
        let capacity = poll_tx.clone();
        let source = Arc::try_new(PollSet::new()).unwrap();
        let control = Arc::try_new(WorkerControl::try_new().unwrap()).unwrap();
        let register_source = source.clone();
        let register: ExternalRegister = Box::try_new(move |waker: &Waker| {
            ExternalRegistration::poll(Arc::clone(&register_source), waker)
        })
        .unwrap();
        let (armed_tx, armed_rx) = mpsc::channel();
        let task_readable = readable.clone();
        let task_control = control.clone();
        let worker = thread::spawn(move || {
            let _exit = WorkerExit(task_control.clone());
            let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
            let mut cx = Context::from_waker(&waker);
            let mut registration = None;
            loop {
                match drive_external_input(&mut reader, &task_readable, &task_control) {
                    ExternalInputAction::Stop => break,
                    ExternalInputAction::Yield => thread::yield_now(),
                    ExternalInputAction::Wait => {
                        match arm_external_input(
                            &reader,
                            &poll_tx,
                            &task_control,
                            &register,
                            &mut registration,
                        ) {
                            Ok(true) => continue,
                            Ok(false) => {}
                            Err(error) => panic!("failed to arm external input: {error}"),
                        }
                        match poll_external_input_wait(&task_control, &mut registration, &mut cx) {
                            Poll::Ready(()) => {
                                drop(registration.take());
                                continue;
                            }
                            Poll::Pending => {
                                armed_tx.send(()).unwrap();
                                thread::park();
                            }
                        }
                    }
                }
            }
        });

        let mut output = Vec::new();
        output.try_reserve_exact(input_len).unwrap();
        for _ in 0..16 {
            armed_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let mut chunk = [0; BUF_SIZE];
            loop {
                let read = buf_rx.pop_slice(&mut chunk);
                if read == 0 {
                    break;
                }
                output.extend_from_slice(&chunk[..read]);
            }
            capacity.clone().wake();
            if source_drained.load(Ordering::Acquire) && output.len() == input_len {
                break;
            }
        }
        assert_eq!(consumed.load(Ordering::Acquire), input_len);
        assert!(source_drained.load(Ordering::Acquire));
        assert_eq!(output.len(), input_len);
        assert!(output.iter().all(|byte| *byte == b'a'));
        control.cancel();
        worker.join().unwrap();
        assert!(control.cancelled.load(Ordering::Acquire));
        assert!(control.terminated.load(Ordering::Acquire));
    }

    #[test]
    fn echo_tail_is_deferred_until_output_capacity_returns() {
        let mut output = Vec::new();
        output.try_reserve_exact(4).unwrap();
        let state = Arc::new(SpinNoIrq::new(EchoState { budget: 0, output }));
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: vec![b'x'],
                    offset: 0,
                    eof: false,
                },
                writer: BoundedSink(state.clone()),
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        assert!(!ldisc.poll_read());
        assert!(state.lock().output.is_empty());
        state.lock().budget = 1;
        assert!(!ldisc.poll_read());
        assert_eq!(state.lock().output.as_slice(), b"x");
    }

    #[test]
    fn canonical_line_is_bounded_and_keeps_delimiter() {
        let mut input = vec![b'a'; CANONICAL_BUF_SIZE + 900];
        input.push(b'\n');
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input,
                    offset: 0,
                    eof: false,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        for _ in 0..128 {
            if ldisc.poll_read() {
                break;
            }
        }
        assert!(ldisc.poll_read());

        let mut output = Vec::new();
        output.try_reserve_exact(CANONICAL_BUF_SIZE).unwrap();
        let mut chunk = [0; 113];
        while output.len() != CANONICAL_BUF_SIZE {
            let read = ldisc.read(&mut chunk).unwrap();
            output.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(output.len(), CANONICAL_BUF_SIZE);
        assert!(output[..CANONICAL_LINE_MAX].iter().all(|ch| *ch == b'a'));
        assert_eq!(output[CANONICAL_LINE_MAX], b'\n');
    }

    #[test]
    fn canonical_staging_reports_progress_beyond_two_raw_chunks() {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: vec![b'a'; BUF_SIZE * 3 + 1],
                    offset: 0,
                    eof: false,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        let Processor::Manual(reader) = &mut ldisc.processor else {
            unreachable!();
        };
        assert!(reader.poll().unwrap());
        assert!(reader.poll().unwrap());
        assert!(reader.poll().unwrap());
    }

    #[test]
    fn empty_veof_is_a_zero_length_record_before_later_input() {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        let veof = terminal.load_termios().special_char(VEOF);
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: vec![veof, b'x', b'\n'],
                    offset: 0,
                    eof: false,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        assert!(ldisc.poll_read());
        let mut output = [0; 8];
        assert_eq!(ldisc.read(&mut output), Ok(0));
        for _ in 0..4 {
            if ldisc.poll_read() {
                break;
            }
        }
        assert_eq!(ldisc.read(&mut output), Ok(2));
        assert_eq!(&output[..2], b"x\n");
    }

    #[test]
    fn noncanonical_vmin_is_capped_by_read_count() {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        terminal.termios.lock().set_canonical_for_test(false);
        terminal.termios.lock().set_special_char_for_test(VMIN, 5);
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: vec![b'a', b'b'],
                    offset: 0,
                    eof: false,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        let mut output = [0; 2];
        assert_eq!(ldisc.read(&mut output), Ok(2));
        assert_eq!(&output, b"ab");
    }

    #[test]
    fn eof_flushes_partial_canonical_input_before_drained_state() {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: b"tail".to_vec(),
                    offset: 0,
                    eof: true,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();

        for _ in 0..4 {
            if ldisc.poll_read() {
                break;
            }
        }
        assert!(!ldisc.input_drained());
        let mut output = [0; 8];
        assert_eq!(ldisc.read(&mut output), Ok(4));
        assert_eq!(&output[..4], b"tail");
        assert!(ldisc.input_drained());
    }
}
