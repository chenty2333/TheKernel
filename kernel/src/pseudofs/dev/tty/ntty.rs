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
pub struct Console;
impl TtyRead for Console {
    fn read(&mut self, buf: &mut [u8]) -> AxResult<usize> {
        Ok(axhal::console::read_bytes(buf))
    }
}
impl TtyWrite for Console {
    fn write(&self, buf: &[u8]) -> AxResult<usize> {
        axhal::console::write_bytes(buf);
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
            reader: Console,
            writer: Console,
            process_mode,
        },
        None,
    )
    .expect("failed to construct console tty")
}
