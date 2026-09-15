use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use axerrno::{AxError, AxResult};
use axpoll::PollSet;
use axsync::Mutex;
use lazy_static::lazy_static;

use super::{
    Tty,
    terminal::ldisc::{ExternalRegistration, ProcessMode, TtyConfig, TtyRead, TtyWrite},
};

pub type NTtyDriver = Tty<Console, Console>;

struct ConsoleOutput {
    stopped: AtomicBool,
    input_stopped: AtomicBool,
    column: AtomicUsize,
    events: Arc<PollSet>,
    /// Serializes one console write against another.
    ///
    /// The two writers that share this state are a process writing to the
    /// terminal and the line discipline echoing typed input back; both reach
    /// the port through [`Console::write`].  The port lock in the platform
    /// console covers a single driver call, but one write can span several of
    /// them (the translation emits a batch at a time) and the output column
    /// below is read-modify-written here, so without a lock over the whole
    /// call two writers can split each other's lines and corrupt the column
    /// that tab stops and erases are computed from.  Linux holds
    /// `tty->atomic_write_lock` over exactly this span.
    write_gate: Mutex<()>,
}

#[derive(Clone)]
pub struct Console {
    terminal: Arc<super::terminal::Terminal>,
    output: Arc<ConsoleOutput>,
    vt: Option<u16>,
    pending: [u8; 80],
    pending_len: usize,
    pending_stamp: super::vt::ConsoleInputStamp,
}

impl Console {
    fn new(vt: Option<u16>, terminal: Arc<super::terminal::Terminal>) -> Self {
        Self {
            terminal,
            output: Arc::new(ConsoleOutput {
                stopped: AtomicBool::new(false),
                input_stopped: AtomicBool::new(false),
                column: AtomicUsize::new(0),
                events: Arc::new(PollSet::new()),
                write_gate: Mutex::new(()),
            }),
            vt,
            pending: [0; 80],
            pending_len: 0,
            pending_stamp: Default::default(),
        }
    }

    fn route_input(
        &mut self,
        read: impl FnOnce(&mut [u8]) -> (usize, super::vt::ConsoleInputStamp),
        route: impl FnOnce(super::vt::ConsoleInputStamp, &[u8]) -> AxResult<()>,
    ) -> AxResult<(usize, bool)> {
        if self.pending_len == 0 {
            (self.pending_len, self.pending_stamp) = read(&mut self.pending);
        }
        if self.pending_len == 0 {
            return Ok((0, false));
        }
        match route(self.pending_stamp, &self.pending[..self.pending_len]) {
            Ok(()) => {
                self.pending_len = 0;
                Ok((0, true))
            }
            // Keep the exact batch and stop reading the hardware until the
            // target discipline publishes capacity or the active VT changes.
            Err(AxError::WouldBlock) => Ok((0, false)),
            // A flush or VT switch revoked this not-yet-delivered batch.
            Err(AxError::Interrupted) => {
                self.pending_len = 0;
                Ok((0, true))
            }
            Err(error) => Err(error),
        }
    }
}
impl TtyRead for Console {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        self.read_with_progress(buf).map(|(read, _)| read)
    }

    fn read_with_progress(&mut self, buf: &mut [u8]) -> AxResult<(usize, bool)> {
        if self.vt.is_some() {
            // A VT's manual line discipline is fed exclusively through
            // route_console_input; the hardware FIFO belongs to the root
            // console below.  Reading it here would steal bytes from that
            // route and discard them.
            return Ok((0, false));
        }
        let _ = buf;
        self.route_input(
            |buf| super::VT_MANAGER.read_console_input(buf, axhal::console::read_bytes),
            |stamp, bytes| super::VT_MANAGER.route_console_input(stamp, bytes),
        )
    }

    fn input_capacity_changed(&self) {
        if self.vt.is_some() {
            wake_console_input();
        }
    }

    fn flush_input(&mut self) {
        self.pending_len = 0;
    }
}
impl Console {
    fn emit(&self, bytes: &[u8]) {
        axhal::console::write_tty_bytes(bytes);
        let vt = self.vt.unwrap_or_else(|| super::VT_MANAGER.active());
        super::fbcon::write(
            vt,
            bytes,
            super::VT_MANAGER.active(),
            super::VT_MANAGER.graphics(vt),
        );
    }
}

impl TtyWrite for Console {
    fn write(&self, buf: &[u8]) -> AxResult<usize> {
        if self.output.stopped.load(Ordering::Acquire)
            || self.output.input_stopped.load(Ordering::Acquire)
        {
            return Err(AxError::WouldBlock);
        }
        // Held until the whole buffer has reached the port, so the bytes of
        // this write stay contiguous and `column` advances once.
        let _write_gate = self.output.write_gate.lock();
        let term = self.terminal.load_termios();
        let mut column = self.output.column.load(Ordering::Acquire);
        let mut batch = [0; 256];
        let mut used = 0;
        for &byte in buf {
            let mut output = [0; 8];
            let count =
                super::terminal::ldisc::process_output_char(&term, &mut column, byte, &mut output);
            if used + count > batch.len() {
                self.emit(&batch[..used]);
                used = 0;
            }
            batch[used..used + count].copy_from_slice(&output[..count]);
            used += count;
        }
        if used != 0 {
            self.emit(&batch[..used]);
        }
        self.output.column.store(column, Ordering::Release);
        Ok(buf.len())
    }

    fn poll_write(&self) -> bool {
        !(self.output.stopped.load(Ordering::Acquire)
            || self.output.input_stopped.load(Ordering::Acquire))
    }
    fn tx_poll_source(&self) -> Option<&Arc<PollSet>> {
        Some(&self.output.events)
    }
    fn set_output_stopped(&self, stopped: bool) {
        self.output.stopped.store(stopped, Ordering::Release);
        self.output.events.wake();
    }
    fn set_input_flow_stopped(&self, stopped: bool) {
        self.output.input_stopped.store(stopped, Ordering::Release);
        self.output.events.wake();
    }
    fn output_column(&self) -> Option<usize> {
        Some(self.output.column.load(Ordering::Acquire))
    }
}

lazy_static! {
    /// The default TTY device.
    pub static ref N_TTY: Arc<NTtyDriver> = new_n_tty();

}

static CONSOLE_INPUT: spin::Once<Arc<PollSet>> = spin::Once::new();

fn console_input_source() -> &'static Arc<PollSet> {
    CONSOLE_INPUT.call_once(|| Arc::new(PollSet::new()))
}

pub(super) fn wake_console_input() {
    if let Some(source) = CONSOLE_INPUT.get() {
        source.wake();
    }
}

fn new_n_tty() -> Arc<NTtyDriver> {
    let terminal = Arc::try_new(super::terminal::Terminal::default())
        .expect("failed to allocate console terminal");
    let process_mode = if let Some(irq) = axhal::console::irq_num() {
        // One stable readiness source covers both fresh UART input and
        // capacity released by the active VT. Initialize it before enabling
        // the IRQ so the handler never initializes or allocates anything.
        let source = console_input_source().clone();
        assert!(
            axhal::irq::register(irq, wake_console_input),
            "console IRQ already owned"
        );
        ProcessMode::External(
            Box::try_new(move |waker: &Waker| ExternalRegistration::poll(source.clone(), waker))
                .map_err(|_| AxError::NoMemory)
                .expect("failed to allocate console tty registration"),
        )
    } else {
        ProcessMode::Manual
    };
    Tty::try_new(
        terminal.clone(),
        TtyConfig {
            reader: Console::new(None, terminal.clone()),
            writer: Console::new(None, terminal),
            process_mode,
        },
        None,
    )
    .expect("failed to construct console tty")
}

/// Builds one independently session-ownable virtual-console terminal.  Input
/// still comes from the hardware console, but termios/job-control/session
/// state belongs to this VT alone.
pub(crate) fn new_virtual_tty(number: u16) -> Arc<NTtyDriver> {
    let terminal =
        Arc::try_new(super::terminal::Terminal::default()).expect("failed to allocate VT terminal");
    Tty::try_new(
        terminal.clone(),
        TtyConfig {
            reader: Console::new(Some(number), terminal.clone()),
            writer: Console::new(Some(number), terminal),
            process_mode: ProcessMode::Manual,
        },
        None,
    )
    .expect("failed to construct virtual-console tty")
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct CountWake(AtomicUsize);
    impl alloc::task::Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn revoked_pending_batch_is_discarded_before_reading_new_input() {
        let mut root = Console::new(None, Arc::new(Default::default()));
        assert_eq!(
            root.route_input(
                |buf| {
                    buf[..3].copy_from_slice(b"old");
                    (3, Default::default())
                },
                |_, _| Err(AxError::WouldBlock)
            )
            .unwrap(),
            (0, false)
        );
        assert_eq!(
            root.route_input(
                |_| panic!("must retain old batch"),
                |_, bytes| {
                    assert_eq!(bytes, b"old");
                    Err(AxError::Interrupted)
                }
            )
            .unwrap(),
            (0, true)
        );
        assert_eq!(root.pending_len, 0);
        assert_eq!(
            root.route_input(
                |buf| {
                    buf[..3].copy_from_slice(b"new");
                    (3, Default::default())
                },
                |_, bytes| {
                    assert_eq!(bytes, b"new");
                    Ok(())
                }
            )
            .unwrap(),
            (0, true)
        );
    }

    #[test]
    fn routed_uart_burst_retains_backpressure_and_retries_on_capacity() {
        let _context = crate::test_support::scheduler_test_context();
        let terminal = Arc::new(super::super::terminal::Terminal::default());
        let mut raw = terminal.termios.lock().to_user_bytes();
        raw[12..16].copy_from_slice(&0u32.to_ne_bytes());
        *terminal.termios.lock() = super::super::terminal::termios::Termios2::from_user_bytes(raw);
        let tty = Tty::try_new(
            terminal,
            TtyConfig {
                reader: Console::new(Some(1), Arc::new(Default::default())),
                writer: Console::new(Some(1), Arc::new(Default::default())),
                process_mode: ProcessMode::Manual,
            },
            None,
        )
        .unwrap();
        let mut root = Console::new(None, Arc::new(Default::default()));
        let mut input = vec![b'x'; 8192];
        // Vary every byte to expose duplicated or reordered retained prefixes.
        for (i, b) in input.iter_mut().enumerate() {
            *b = b'a' + (i % 26) as u8;
        }
        let mut offset = 0;
        let mut read_calls = 0;
        let mut route = |root: &mut Console| {
            root.route_input(
                |buf| {
                    read_calls += 1;
                    let count = buf.len().min(input.len() - offset);
                    buf[..count].copy_from_slice(&input[offset..offset + count]);
                    offset += count;
                    (count, Default::default())
                },
                |_, bytes| tty.inject_input(bytes),
            )
            .unwrap()
        };
        let mut accepted_batches = 0;
        while route(&mut root).1 {
            accepted_batches += 1;
            assert!(
                accepted_batches < input.len() / 80,
                "bounded discipline must apply backpressure"
            );
        }
        assert!(accepted_batches > 0);
        assert!(!route(&mut root).1);
        drop(route);
        assert_eq!(
            read_calls,
            accepted_batches + 1,
            "blocked batch must not read or discard more UART data"
        );
        assert_eq!(root.pending_len, 80);
        let wake = Arc::new(CountWake(AtomicUsize::new(0)));
        let _token = console_input_source()
            .register(&Waker::from(wake.clone()))
            .unwrap();
        let mut output = Vec::new();
        let mut chunk = [0; 7];
        while output.len() < input.len() {
            let n = tty.ldisc.lock().read(&mut chunk).unwrap();
            output.extend_from_slice(&chunk[..n]);
            root.route_input(
                |buf| {
                    let count = buf.len().min(input.len() - offset);
                    buf[..count].copy_from_slice(&input[offset..offset + count]);
                    offset += count;
                    (count, Default::default())
                },
                |_, bytes| tty.inject_input(bytes),
            )
            .unwrap();
        }
        assert_ne!(
            wake.0.load(Ordering::Relaxed),
            0,
            "VT consumption must wake the blocked router"
        );
        assert_eq!(offset, input.len());
        assert_eq!(root.pending_len, 0);
        assert_eq!(output, input);
    }
}
