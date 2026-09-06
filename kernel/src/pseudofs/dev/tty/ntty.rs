use alloc::{boxed::Box, sync::Arc};
use core::task::Waker;

use axerrno::{AxError, AxResult};
use lazy_static::lazy_static;

use super::{
    Tty,
    terminal::ldisc::{ExternalRegistration, ProcessMode, TtyConfig, TtyRead, TtyWrite},
};

pub type NTtyDriver = Tty<Console, Console>;

#[derive(Clone, Copy)]
pub struct Console {
    vt: Option<u16>,
}
impl TtyRead for Console {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        self.read_with_progress(buf).map(|(read, _)| read)
    }

    fn read_with_progress(&mut self, buf: &mut [u8]) -> AxResult<(usize, bool)> {
        if self.vt.is_some() {
            // A VT's manual line discipline is fed exclusively through
            // route_active_input; the hardware FIFO belongs to the root
            // console below.  Reading it here would steal bytes from that
            // route and discard them.
            return Ok((0, false));
        }
        let read = axhal::console::read_bytes(buf);
        if read != 0 {
            // N_TTY owns the hardware IRQ/read side.  VTs never call this
            // transport directly; they receive only the active console's
            // bytes through their own manual line disciplines.
            // Input is bounded by the target discipline.  Backpressure means
            // the active terminal is saturated; drop that batch rather than
            // killing the sole hardware IRQ reader or leaking it to another
            // VT.
            let _ = super::VT_MANAGER.route_active_input(&buf[..read]);
        }
        // Routing consumed hardware bytes even though this root discipline
        // receives none. Recheck the UART before sleeping: a full batch may
        // leave its edge-triggered IRQ asserted with more input still pending.
        Ok((0, read != 0))
    }
}
impl TtyWrite for Console {
    fn write(&self, buf: &[u8]) -> AxResult<usize> {
        axhal::console::write_bytes(buf);
        let vt = self.vt.unwrap_or_else(|| super::VT_MANAGER.active());
        super::fbcon::write(
            vt,
            buf,
            super::VT_MANAGER.active(),
            super::VT_MANAGER.graphics(vt),
        );
        Ok(buf.len())
    }
}

lazy_static! {
    /// The default TTY device.
    pub static ref N_TTY: Arc<NTtyDriver> = new_n_tty();
}

fn new_n_tty() -> Arc<NTtyDriver> {
    let terminal = Arc::try_new(Default::default()).expect("failed to allocate console terminal");
    let process_mode = if let Some(irq) = axhal::console::irq_num() {
        // The console device is the IRQ capability owner. The generic waiter
        // registry only attaches and cancels callbacks.
        axhal::irq::set_enable(irq, true);
        ProcessMode::External(
            Box::try_new(move |waker: &Waker| ExternalRegistration::irq(irq, waker))
                .map_err(|_| AxError::NoMemory)
                .expect("failed to allocate console tty registration"),
        )
    } else {
        ProcessMode::Manual
    };
    Tty::try_new(
        terminal,
        TtyConfig {
            reader: Console { vt: None },
            writer: Console { vt: None },
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
    Tty::try_new(
        Arc::try_new(Default::default()).expect("failed to allocate VT terminal"),
        TtyConfig {
            reader: Console { vt: Some(number) },
            writer: Console { vt: Some(number) },
            process_mode: ProcessMode::Manual,
        },
        None,
    )
    .expect("failed to construct virtual-console tty")
}
