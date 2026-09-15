use alloc::{boxed::Box, collections::vec_deque::VecDeque, string::String, sync::Arc, vec::Vec};
use core::{
    future::poll_fn,
    ops::Range,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering},
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
    ECHOCTL, ECHOE, ECHOK, ECHOKE, ECHONL, ICRNL, IEXTEN, IGNCR, INLCR, ISIG, ISTRIP, IUCLC, IUTF8,
    IXANY, IXON, NOFLSH, OCRNL, OLCUC, ONLCR, ONLRET, ONOCR, OPOST, VEOF, VERASE, VKILL, VLNEXT,
    VMIN, VREPRINT, VSTART, VSTOP, VTIME, VWERASE,
};
use ringbuf::{
    CachingCons, CachingProd,
    traits::{Consumer, Observer, Producer, Split},
};
use tk_linux_signal::SignalInfo;

use super::{Terminal, termios::Termios2};
use crate::task::send_signal_to_process_group;

const BUF_SIZE: usize = 80;
const CANONICAL_BUF_SIZE: usize = 4096;
const CANONICAL_LINE_MAX: usize = CANONICAL_BUF_SIZE - 1;
const ECHO_BUF_SIZE: usize = 4096;
const EXTERNAL_PROGRESS_BUDGET: usize = 64;

const READ_BUF_SIZE: usize = 4096;
type ReadBuf = Arc<ringbuf::StaticRb<u8, READ_BUF_SIZE>>;
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

    /// Read local bytes and report transport progress separately. A hardware
    /// console can route bytes to another discipline without publishing them
    /// here; that work must still keep the external worker draining its source.
    fn read_with_progress(&mut self, buf: &mut [u8]) -> AxResult<(usize, bool)> {
        self.read(buf).map(|read| (read, read != 0))
    }

    /// Notify an owning transport that userspace has freed input capacity.
    fn input_capacity_changed(&self) {}

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

    /// Column after accepted output, if tracked by this transport. Echo erase
    /// uses this to preserve a prompt before an expanded tab.
    fn output_column(&self) -> Option<usize> {
        None
    }

    /// Apply local software output flow control. A stopped writer must apply
    /// backpressure until it is resumed, rather than pretending output drained.
    fn set_output_stopped(&self, _stopped: bool) {}

    /// IXON stop/start is independent of an explicit tcflow(TCOOFF).
    fn set_input_flow_stopped(&self, _stopped: bool) {}
}

/// Tracks the terminal's conventional byte columns after output processing.
/// ANSI cursor addressing belongs to the display emulator, not N_TTY.
pub(crate) fn output_column_after(mut column: usize, bytes: &[u8]) -> usize {
    for &byte in bytes {
        match byte {
            b'\r' => column = 0,
            b'\x08' => column = column.saturating_sub(1),
            b'\t' => column = column.saturating_add(8 - column % 8),
            byte if !byte.is_ascii_control() && byte & 0xc0 != 0x80 => {
                column = column.saturating_add(1)
            }
            _ => {}
        }
    }
    column
}

/// One N_TTY output byte expands to at most one tab stop (eight spaces).
/// Callers retain a short pending suffix when the transport applies pressure.
pub(crate) fn process_output_char(
    term: &Termios2,
    column: &mut usize,
    mut byte: u8,
    output: &mut [u8; 8],
) -> usize {
    if !term.has_oflag(OPOST) {
        output[0] = byte;
        *column = output_column_after(*column, &output[..1]);
        return 1;
    }
    if term.has_oflag(OLCUC) {
        byte = byte.to_ascii_uppercase();
    }
    match byte {
        b'\n' => {
            if term.has_oflag(ONLCR) {
                output[..2].copy_from_slice(b"\r\n");
                *column = 0;
                return 2;
            }
            if term.has_oflag(ONLRET) {
                *column = 0;
            }
        }
        b'\r' => {
            if term.has_oflag(ONOCR) && *column == 0 {
                return 0;
            }
            if term.has_oflag(OCRNL) {
                output[0] = b'\n';
                if term.has_oflag(ONLRET) {
                    *column = 0;
                }
                return 1; // OCRNL does not recursively apply ONLCR.
            }
            *column = 0;
        }
        b'\t' if term.output_tab_expansion() => {
            let count = 8 - *column % 8;
            output[..count].fill(b' ');
            *column = column.saturating_add(count);
            return count;
        }
        _ => {}
    }
    output[0] = byte;
    *column = output_column_after(*column, &output[..1]);
    1
}

struct InputReader<R, W> {
    terminal: Arc<Terminal>,
    reader: R,
    writer: W,
    buf_tx: CachingProd<ReadBuf>,
    produced: usize,
    discard_before: Arc<AtomicUsize>,
    read_buf: [u8; BUF_SIZE],
    read_range: Range<usize>,
    // Linux N_TTY bounds a canonical input line to 4096 bytes. Reserving the
    // entire limit during admission means input processing never allocates.
    line_buf: Vec<u8>,
    line_widths: Vec<u8>,
    line_read: Option<usize>,
    literal_next: bool,
    ixon_stopped: bool,
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
        self.line_widths.clear();
        self.line_read = None;
        self.literal_next = false;
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
            let (read, transport_progress) = self.reader.read_with_progress(&mut self.read_buf)?;
            self.read_range = 0..read;
            progressed |= read != 0 || transport_progress;
        }
        let term = self.terminal.load_termios();
        if self.ixon_stopped && !term.has_iflag(IXON) {
            self.ixon_stopped = false;
            self.writer.set_input_flow_stopped(false);
        }
        if !term.has_lflag(IEXTEN) {
            self.literal_next = false;
        }
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
                self.produced = self.produced.wrapping_add(read);
                if read == 0 {
                    break;
                }
                progressed = true;
                *offset += read;
                if *offset == self.line_buf.len() {
                    self.line_read = None;
                    self.line_buf.clear();
                    self.line_widths.clear();
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

            if term.has_iflag(ISTRIP) {
                ch &= 0x7f;
            }
            if term.has_iflag(IUCLC) && term.has_lflag(IEXTEN) {
                ch = ch.to_ascii_lowercase();
            }
            let literal = core::mem::take(&mut self.literal_next);
            if !literal {
                if ch == b'\r' {
                    if term.has_iflag(IGNCR) {
                        continue;
                    }
                    if term.has_iflag(ICRNL) {
                        ch = b'\n';
                    }
                } else if ch == b'\n' && term.has_iflag(INLCR) {
                    ch = b'\r';
                }
                if term.has_iflag(IXON) {
                    if term.matches_special_char(VSTOP, ch) {
                        self.ixon_stopped = true;
                        self.writer.set_input_flow_stopped(true);
                        continue;
                    }
                    if term.matches_special_char(VSTART, ch) {
                        self.ixon_stopped = false;
                        self.writer.set_input_flow_stopped(false);
                        continue;
                    }
                    if self.ixon_stopped && term.has_iflag(IXANY) {
                        self.ixon_stopped = false;
                        self.writer.set_input_flow_stopped(false);
                    }
                }
                if self.check_send_signal(&term, ch) {
                    continue;
                }
            }
            if !term.canonical() {
                if term.echo() {
                    self.output_char(&term, ch);
                }
                if self.buf_tx.try_push(ch).is_ok() {
                    self.produced = self.produced.wrapping_add(1);
                }
                continue;
            }
            if !literal {
                if term.has_lflag(IEXTEN) && term.matches_special_char(VLNEXT, ch) {
                    self.literal_next = true;
                    if term.echo() && term.has_lflag(ECHOCTL) {
                        self.queue_echo(b"^\x08");
                    }
                    continue;
                }
                if term.matches_special_char(VERASE, ch) {
                    self.erase_character(&term, false);
                    continue;
                }
                if term.matches_special_char(VKILL, ch) {
                    if self.line_buf.is_empty() {
                        continue;
                    }
                    if term.echo()
                        && !(term.has_lflag(ECHOK)
                            && term.has_lflag(ECHOKE)
                            && term.has_lflag(ECHOE))
                    {
                        self.line_buf.clear();
                        self.line_widths.clear();
                        self.output_char(&term, ch);
                        if term.has_lflag(ECHOK) {
                            self.queue_echo(b"\n");
                        }
                    } else {
                        while !self.line_buf.is_empty() {
                            self.erase_character(&term, true);
                        }
                    }
                    continue;
                }
                if term.has_lflag(IEXTEN) && term.matches_special_char(VWERASE, ch) {
                    let mut seen_word = false;
                    while let Some(&last) = self.line_buf.last() {
                        if last.is_ascii_alphanumeric() || last == b'_' {
                            seen_word = true;
                        } else if seen_word {
                            break;
                        }
                        self.erase_character(&term, true);
                    }
                    continue;
                }
                if term.has_lflag(IEXTEN) && term.matches_special_char(VREPRINT, ch) {
                    if term.echo() {
                        self.output_char(&term, ch);
                        self.queue_echo(b"\n");
                        for i in 0..self.line_buf.len() {
                            self.output_char(&term, self.line_buf[i]);
                        }
                    }
                    continue;
                }
            }

            let is_veof = !literal && term.matches_special_char(VEOF, ch);
            if (!literal && term.is_eol(ch)) || is_veof {
                if !is_veof && self.line_buf.len() < CANONICAL_BUF_SIZE {
                    if term.echo() || (ch == b'\n' && term.has_lflag(ECHONL)) {
                        self.output_char(&term, ch);
                    }
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
            if self.line_buf.len() < CANONICAL_LINE_MAX {
                let column = self
                    .writer
                    .output_column()
                    .map(|column| output_column_after(column, self.echo_buf.make_contiguous()));
                let width = if ch == b'\t' {
                    // Without a transport column snapshot, retreat only one
                    // column rather than risk erasing an application prompt.
                    column.map_or(1, |column| 8 - column % 8)
                } else if ch.is_ascii_control() {
                    if term.has_lflag(ECHOCTL) { 2 } else { 0 }
                } else if term.has_iflag(IUTF8) && ch & 0xc0 == 0x80 {
                    0
                } else {
                    1
                };
                self.line_widths
                    .push(if term.echo() { width as u8 } else { 0 });
                if term.echo() {
                    self.output_char(&term, ch);
                }
                self.line_buf.push(ch);
            } else if term.echo() {
                // Linux N_TTY rings on canonical overflow independently of
                // the retained IMAXBEL flag; control processing stays live.
                self.queue_echo(b"\x07");
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

    fn check_send_signal(&mut self, term: &Termios2, ch: u8) -> bool {
        if !term.has_lflag(ISIG) {
            return false;
        }
        let Some(signo) = term.signo_for(ch) else {
            return false;
        };
        if !term.has_lflag(NOFLSH) {
            self.line_buf.clear();
            self.line_widths.clear();
            self.literal_next = false;
            self.line_read = None;
            self.echo_buf.clear();
            self.echo_pending.store(0, Ordering::Release);
            self.empty_eof_pending.store(false, Ordering::Release);
            // Only the consumer may advance the SPSC ring's read cursor.
            // Publish the pre-signal prefix to discard; later input in this
            // transport batch remains readable, just as in Linux N_TTY.
            self.discard_before.store(self.produced, Ordering::Release);
            self.writer.flush_output();
        }
        if term.echo() {
            self.output_char(term, ch);
        }
        if let Some(pg) = self.terminal.job_control.foreground() {
            let sig = SignalInfo::new_kernel(signo);
            if let Err(err) = send_signal_to_process_group(pg.pgid(), Some(sig)) {
                warn!("Failed to send signal: {err:?}");
            }
        }
        // Signal characters never become input bytes, including in cbreak
        // mode and when there is currently no foreground process group.
        true
    }

    fn erase_character(&mut self, term: &Termios2, visual: bool) {
        let Some(mut erased) = self.line_buf.pop() else {
            return;
        };
        let mut width = self.line_widths.pop().unwrap_or(0);
        if term.has_iflag(IUTF8) {
            while erased & 0xc0 == 0x80 {
                let Some(previous) = self.line_buf.pop() else {
                    break;
                };
                width = width.saturating_add(self.line_widths.pop().unwrap_or(0));
                erased = previous;
            }
        }
        if !term.echo() {
            return;
        }
        if !visual && !term.has_lflag(ECHOE) {
            self.output_char(term, term.special_char(VERASE));
        } else {
            for _ in 0..width {
                if erased == b'\t' {
                    self.queue_echo(b"\x08");
                } else {
                    self.queue_echo(b"\x08 \x08");
                }
            }
        }
    }

    fn output_char(&mut self, term: &Termios2, ch: u8) {
        if ch.is_ascii_control() && !matches!(ch, b'\n' | b'\t') && term.has_lflag(ECHOCTL) {
            self.queue_echo(&[b'^', ch ^ 0x40]);
        } else {
            self.queue_echo(&[ch]);
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
    pending: [u8; 8],
    pending_range: Range<usize>,
    output_column: usize,
    buf_tx: CachingProd<ReadBuf>,
}

impl<R: TtyRead> SimpleReader<R> {
    fn poll(&mut self) -> AxResult<()> {
        if self.read_range.is_empty() {
            let read = self.reader.read(&mut self.read_buf)?;
            self.read_range = 0..read;
        }

        let term = self.terminal.load_termios();
        loop {
            while !self.pending_range.is_empty() {
                if self
                    .buf_tx
                    .try_push(self.pending[self.pending_range.start])
                    .is_err()
                {
                    return Ok(());
                }
                self.pending_range.start += 1;
            }
            if self.buf_tx.is_full() || self.read_range.is_empty() {
                break;
            }
            let ch = self.read_buf[self.read_range.start];
            self.read_range.start += 1;
            let count = process_output_char(&term, &mut self.output_column, ch, &mut self.pending);
            self.pending_range = 0..count;
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
    input_generation: u64,
    terminal: Arc<Terminal>,
    buf_rx: CachingCons<ReadBuf>,
    consumed: usize,
    discard_before: Arc<AtomicUsize>,
    poll_tx: Arc<PollSet>,
    empty_eof_pending: Arc<AtomicBool>,
    source_drained: Arc<AtomicBool>,
    echo_pending: Arc<core::sync::atomic::AtomicUsize>,
    processor: Processor<R, W>,
}

impl<R: TtyRead, W: TtyWrite> LineDiscipline<R, W> {
    pub fn try_new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> AxResult<Self> {
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, READ_BUF_SIZE>::default())
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
        let mut line_widths = Vec::new();
        line_widths
            .try_reserve_exact(CANONICAL_BUF_SIZE)
            .map_err(|_| AxError::NoMemory)?;
        let mut echo_buf = VecDeque::new();
        echo_buf
            .try_reserve_exact(ECHO_BUF_SIZE)
            .map_err(|_| AxError::NoMemory)?;
        let discard_before = Arc::try_new(AtomicUsize::new(0)).map_err(|_| AxError::NoMemory)?;
        let reader = InputReader {
            terminal: terminal.clone(),
            reader: config.reader,
            writer: config.writer,
            buf_tx,
            produced: 0,
            discard_before: discard_before.clone(),
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_widths,
            line_read: None,
            literal_next: false,
            ixon_stopped: false,
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
                    pending: [0; 8],
                    pending_range: 0..0,
                    output_column: 0,
                    buf_tx: reader.buf_tx,
                },
                poll_rx,
            ),
        };
        Ok(Self {
            input_generation: 0,
            terminal,
            buf_rx,
            consumed: 0,
            discard_before,
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
        if self.empty_eof_pending.load(Ordering::Acquire) {
            return true;
        }
        let term = self.terminal.load_termios();
        let threshold = if matches!(self.processor, Processor::None(_, _))
            || term.canonical()
            || term.special_char(VTIME) != 0
        {
            1
        } else {
            usize::from(term.special_char(VMIN)).max(1)
        };
        self.buf_rx.occupied_len() >= threshold
    }

    fn refill_read_buffer(&mut self) -> AxResult<()> {
        match &mut self.processor {
            Processor::Manual(reader) => {
                // Drain already available chunks before reporting readiness;
                // otherwise VMIN > the transport chunk size can strand data
                // behind a level-triggered poll with no future producer wake.
                for _ in 0..EXTERNAL_PROGRESS_BUDGET {
                    if !reader.poll()? {
                        break;
                    }
                }
            }
            Processor::None(reader, _) => reader.poll()?,
            Processor::External(processor) => {
                if let Some(error) = processor.control.failure() {
                    return Err(error);
                }
            }
        }
        self.discard_signal_prefix();
        Ok(())
    }

    fn discard_signal_prefix(&mut self) {
        let before = self.discard_before.load(Ordering::Acquire);
        let count = before.wrapping_sub(self.consumed);
        // At most the ring capacity can be outstanding. A wrapped difference
        // larger than that means the consumer already passed this cutoff.
        if count <= BUF_SIZE && count != 0 {
            let skipped = self.buf_rx.skip(count);
            self.consumed = self.consumed.wrapping_add(skipped);
            self.poll_tx.wake();
        }
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

    pub(crate) fn input_generation(&self) -> u64 {
        self.input_generation
    }

    /// The caller holds this discipline's lock across generation validation
    /// and injection, so TCIFLUSH cannot be followed by an old retained batch.
    pub(crate) fn inject_input_at(&mut self, bytes: &[u8], generation: u64) -> AxResult<()> {
        if generation != self.input_generation {
            return Err(AxError::Interrupted);
        }
        self.inject_input(bytes)
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
        // A userspace read may have freed public-ring space since the last
        // injection. Drain the retained staging prefix before admission.
        reader.poll()?;
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
        self.input_generation = self
            .input_generation
            .checked_add(1)
            .ok_or(AxError::OutOfRange)?;
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
                reader.reader.input_capacity_changed();
            }
            Processor::None(reader, _) => {
                reader.reader.flush_input();
                reader.read_range = 0..0;
                reader.pending_range = 0..0;
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
        loop {
            let read = self.buf_rx.pop_slice(&mut discarded);
            self.consumed = self.consumed.wrapping_add(read);
            if read == 0 {
                break;
            }
        }
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
            if let Processor::Manual(reader) = &self.processor {
                reader.reader.input_capacity_changed();
            }
            self.poll_tx.wake();
            return Ok(0);
        }
        if term.canonical() && self.buf_rx.is_empty() {
            return Err(AxError::WouldBlock);
        }

        let total_read = self.buf_rx.pop_slice(buf);
        self.consumed = self.consumed.wrapping_add(total_read);
        if total_read != 0
            && let Processor::Manual(reader) = &self.processor
        {
            reader.reader.input_capacity_changed();
        }
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
    fn output_processing_handles_common_stty_flags() {
        let mut term = Termios2::default();
        edit_flags(
            &mut term,
            4,
            ONOCR | OCRNL | OLCUC | ONLRET | linux_raw_sys::general::TAB3,
            0,
        );
        let mut column = 0;
        let mut output = Vec::new();
        for &byte in b"\ra\tb\n" {
            let mut chunk = [0; 8];
            let count = process_output_char(&term, &mut column, byte, &mut chunk);
            output.extend_from_slice(&chunk[..count]);
        }
        assert_eq!(output, b"A       B\r\n");
        assert_eq!(column, 0);
        column = 4;
        let mut chunk = [0; 8];
        assert_eq!(
            process_output_char(&term, &mut column, b'\r', &mut chunk),
            1
        );
        assert_eq!(chunk[0], b'\n');
        assert_eq!(column, 0);
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
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, READ_BUF_SIZE>::default()).unwrap();
        let (buf_tx, _buf_rx) = read_buf.split();
        let mut line_buf = Vec::new();
        line_buf.try_reserve_exact(CANONICAL_BUF_SIZE).unwrap();
        let mut line_widths = Vec::new();
        line_widths
            .try_reserve_exact(CANONICAL_BUF_SIZE)
            .unwrap_or_else(|_| panic!("tty echo width admission"));
        let mut echo_buf = VecDeque::new();
        echo_buf.try_reserve_exact(ECHO_BUF_SIZE).unwrap();
        let reads = Arc::try_new(AtomicUsize::new(0)).unwrap();
        let mut reader = InputReader {
            terminal: Arc::try_new(Terminal::default()).unwrap(),
            reader: EndlessKillReader(reads.clone()),
            writer: Sink,
            buf_tx,
            produced: 0,
            discard_before: Arc::new(AtomicUsize::new(0)),
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_widths,
            line_read: None,
            literal_next: false,
            ixon_stopped: false,
            echo_buf,
            echo_pending: Arc::try_new(AtomicUsize::new(0)).unwrap(),
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
    fn routed_input_drains_without_duplicate_delivery_and_respects_budget() {
        struct RoutedReader {
            remaining: usize,
            routed: usize,
        }

        impl TtyRead for RoutedReader {
            fn read(&mut self, _buf: &mut [u8]) -> AxResult<usize> {
                panic!("routed transport requires progress-aware reads")
            }

            fn read_with_progress(&mut self, buf: &mut [u8]) -> AxResult<(usize, bool)> {
                let consumed = self.remaining.min(buf.len());
                buf[..consumed].fill(b'a');
                self.remaining -= consumed;
                self.routed += consumed;
                Ok((0, consumed != 0))
            }
        }

        let total = BUF_SIZE * (EXTERNAL_PROGRESS_BUDGET + 1);
        let mut ldisc = LineDiscipline::try_new(
            Arc::try_new(Terminal::default()).unwrap(),
            TtyConfig {
                reader: RoutedReader {
                    remaining: total,
                    routed: 0,
                },
                writer: Sink,
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();
        let Processor::Manual(reader) = &mut ldisc.processor else {
            unreachable!();
        };
        let readable = Arc::new(PollSet::new());
        let control = Arc::new(WorkerControl::try_new().unwrap());

        assert_eq!(
            drive_external_input(reader, &readable, &control),
            ExternalInputAction::Yield
        );
        assert_eq!(reader.reader.routed, BUF_SIZE * EXTERNAL_PROGRESS_BUDGET);
        // No further IRQ is needed to drain the final batch and observe empty.
        assert_eq!(
            drive_external_input(reader, &readable, &control),
            ExternalInputAction::Wait
        );
        assert_eq!(reader.reader.routed, total);
        assert!(reader.read_range.is_empty());
        assert!(reader.line_buf.is_empty());
        assert!(ldisc.buf_rx.is_empty());
    }

    #[test]
    fn external_worker_drains_all_stages_then_cancels_and_joins() {
        let read_buf = Arc::try_new(ringbuf::StaticRb::<u8, READ_BUF_SIZE>::default()).unwrap();
        let (buf_tx, mut buf_rx) = read_buf.split();
        let mut line_buf = Vec::new();
        line_buf.try_reserve_exact(CANONICAL_BUF_SIZE).unwrap();
        let mut line_widths = Vec::new();
        line_widths
            .try_reserve_exact(CANONICAL_BUF_SIZE)
            .unwrap_or_else(|_| panic!("tty echo width admission"));
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
            produced: 0,
            discard_before: Arc::new(AtomicUsize::new(0)),
            read_buf: [0; BUF_SIZE],
            read_range: 0..0,
            line_buf,
            line_widths,
            line_read: None,
            literal_next: false,
            ixon_stopped: false,
            echo_buf,
            echo_pending: Arc::try_new(AtomicUsize::new(0)).unwrap(),
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

    #[derive(Clone)]
    struct RecordingSink {
        output: Arc<SpinNoIrq<Vec<u8>>>,
        stopped: Arc<AtomicBool>,
    }
    impl TtyWrite for RecordingSink {
        fn write(&self, bytes: &[u8]) -> AxResult<usize> {
            if self.stopped.load(Ordering::Acquire) {
                return Err(AxError::WouldBlock);
            }
            self.output.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn set_input_flow_stopped(&self, stopped: bool) {
            self.stopped.store(stopped, Ordering::Release);
        }
        fn output_column(&self) -> Option<usize> {
            Some(output_column_after(0, &self.output.lock()))
        }
    }

    fn input_with_echo(
        input: &[u8],
        flags: impl FnOnce(&mut Termios2),
    ) -> (LineDiscipline<VecReader, RecordingSink>, RecordingSink) {
        let terminal = Arc::new(Terminal::default());
        flags(&mut terminal.termios.lock());
        let sink = RecordingSink {
            output: Arc::new(SpinNoIrq::new(Vec::new())),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let discipline = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: input.to_vec(),
                    offset: 0,
                    eof: false,
                },
                writer: sink.clone(),
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();
        (discipline, sink)
    }

    fn edit_flags(term: &mut Termios2, offset: usize, set: u32, clear: u32) {
        let mut bytes = term.to_user_bytes();
        let flags = u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
        bytes[offset..offset + 4].copy_from_slice(&((flags | set) & !clear).to_ne_bytes());
        *term = Termios2::from_user_bytes(bytes);
    }

    #[test]
    fn canonical_preserves_tabs_nuls_controls_and_non_ascii() {
        let input = b"a\t\0\x01\x1b\x80\xff\n";
        let (mut discipline, sink) = input_with_echo(input, |_| {});
        let mut data = [0; 32];
        assert_eq!(discipline.read(&mut data), Ok(input.len()));
        assert_eq!(&data[..input.len()], input);
        assert_eq!(sink.output.lock().as_slice(), b"a\t^@^A^[\x80\xff\n");
    }

    #[test]
    fn empty_erase_and_kill_never_erase_the_prompt() {
        let (mut discipline, sink) = input_with_echo(b"\x7f\x15", |_| {});
        sink.output.lock().extend_from_slice(b"prompt> ");
        assert!(!discipline.poll_read());
        assert_eq!(sink.output.lock().as_slice(), b"prompt> ");
    }

    #[test]
    fn canonical_visual_erase_handles_control_width_tab_and_utf8() {
        let (mut discipline, sink) = input_with_echo(b"\t\x7f\x01\x7f\xc3\xa9\x7f\n", |term| {
            edit_flags(term, 0, IUTF8, 0)
        });
        sink.output.lock().extend_from_slice(b"> ");
        let mut data = [0; 8];
        assert_eq!(discipline.read(&mut data), Ok(1));
        assert_eq!(data[0], b'\n');
        assert_eq!(
            sink.output.lock().as_slice(),
            b"> \t\x08\x08\x08\x08\x08\x08^A\x08 \x08\x08 \x08\xc3\xa9\x08 \x08\n"
        );
    }

    #[test]
    fn extended_editing_word_kill_and_literal_next_have_real_semantics() {
        let (mut discipline, _) = input_with_echo(b"one two\x17X\x15\x16\x03\n", |_| {});
        let mut data = [0; 32];
        assert_eq!(discipline.read(&mut data), Ok(2));
        assert_eq!(&data[..2], b"\x03\n");
        let (mut discipline, sink) = input_with_echo(b"abc\x15", |_| {});
        assert!(!discipline.poll_read());
        assert_eq!(
            sink.output.lock().as_slice(),
            b"abc\x08 \x08\x08 \x08\x08 \x08"
        );
    }

    #[test]
    fn echonl_echoes_only_newline_with_echo_disabled() {
        let (mut discipline, sink) = input_with_echo(b"abc\n", |term| {
            edit_flags(term, 12, ECHONL, linux_raw_sys::general::ECHO)
        });
        assert!(discipline.poll_read());
        assert_eq!(sink.output.lock().as_slice(), b"\n");
    }

    #[test]
    fn ixon_consumes_stop_start_and_resumes_deferred_echo() {
        let (mut discipline, sink) = input_with_echo(b"\x13abc\n", |_| {});
        assert!(discipline.poll_read());
        assert!(sink.stopped.load(Ordering::Acquire));
        assert!(sink.output.lock().is_empty());
        discipline.inject_input(b"\x11").unwrap();
        assert!(!sink.stopped.load(Ordering::Acquire));
        discipline.poll_read();
        assert_eq!(sink.output.lock().as_slice(), b"abc\n");
        let mut data = [0; 16];
        assert_eq!(discipline.read(&mut data), Ok(4));
        assert_eq!(&data[..4], b"abc\n");
    }

    #[test]
    fn noncanonical_poll_uses_vmin_only_without_vtime() {
        let (mut discipline, _) = input_with_echo(b"ab", |term| {
            term.set_canonical_for_test(false);
            term.set_special_char_for_test(VMIN, 3);
        });
        assert!(!discipline.poll_read());
        discipline.inject_input(b"c").unwrap();
        assert!(discipline.poll_read());
        let (mut timed, _) = input_with_echo(b"a", |term| {
            term.set_canonical_for_test(false);
            term.set_special_char_for_test(VMIN, 255);
            term.set_special_char_for_test(VTIME, 1);
        });
        assert!(timed.poll_read());
        let (mut full_minimum, _) = input_with_echo(&[b'a'; 255], |term| {
            term.set_canonical_for_test(false);
            term.set_special_char_for_test(VMIN, 255);
        });
        assert!(full_minimum.poll_read());
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

    fn signal_input(canonical: bool, isig: bool, noflsh: bool) -> (Vec<u8>, usize) {
        struct SignalSink(Arc<AtomicUsize>);
        impl TtyWrite for SignalSink {
            fn write(&self, bytes: &[u8]) -> AxResult<usize> {
                Ok(bytes.len())
            }
            fn flush_output(&self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let terminal = Arc::new(Terminal::default());
        let mut bytes = terminal.load_termios().to_user_bytes();
        let mut flags = u32::from_ne_bytes(bytes[12..16].try_into().unwrap());
        flags &= !(linux_raw_sys::general::ICANON | ISIG | NOFLSH | linux_raw_sys::general::ECHO);
        if canonical {
            flags |= linux_raw_sys::general::ICANON;
        }
        if isig {
            flags |= ISIG;
        }
        if noflsh {
            flags |= NOFLSH;
        }
        bytes[12..16].copy_from_slice(&flags.to_ne_bytes());
        *terminal.termios.lock() = Termios2::from_user_bytes(bytes);
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut ldisc = LineDiscipline::try_new(
            terminal,
            TtyConfig {
                reader: VecReader {
                    input: b"old\x03new\n".to_vec(),
                    offset: 0,
                    eof: false,
                },
                writer: SignalSink(flushes.clone()),
                process_mode: ProcessMode::Manual,
            },
        )
        .unwrap();
        let mut output = [0; 32];
        let read = ldisc.read(&mut output).unwrap();
        (output[..read].to_vec(), flushes.load(Ordering::Relaxed))
    }

    #[test]
    fn cbreak_signals_consume_control_bytes_and_flush_only_the_prior_prefix() {
        assert_eq!(signal_input(false, true, false), (b"new\n".to_vec(), 1));
        assert_eq!(signal_input(false, true, true), (b"oldnew\n".to_vec(), 0));
        assert_eq!(
            signal_input(false, false, false),
            (b"old\x03new\n".to_vec(), 0)
        );
    }

    #[test]
    fn canonical_signals_flush_partial_line_and_respect_noflsh() {
        assert_eq!(signal_input(true, true, false), (b"new\n".to_vec(), 1));
        assert_eq!(signal_input(true, true, true), (b"oldnew\n".to_vec(), 0));
    }

    #[test]
    fn noncanonical_vmin_is_capped_by_read_count() {
        let terminal = Arc::try_new(Terminal::default()).unwrap();
        terminal.termios.lock().set_canonical_for_test(false);
        terminal
            .termios
            .lock()
            .set_special_char_for_test(linux_raw_sys::general::VMIN, 5);
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
