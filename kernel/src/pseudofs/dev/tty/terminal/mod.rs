//! Terminal module.

use core::sync::atomic::{AtomicBool, AtomicU32};

use axpoll::PollSet;
use bytemuck::AnyBitPattern;
use kspin::{SpinNoIrq, SpinNoPreempt};

pub mod job;
pub mod ldisc;
pub mod termios;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, AnyBitPattern)]
pub struct WindowSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

pub struct Terminal {
    /// Serializes controlling-terminal ownership changes with a hangup.
    pub lifecycle: SpinNoIrq<()>,
    pub job_control: job::JobControl,
    pub window_size: SpinNoPreempt<WindowSize>,
    pub termios: SpinNoPreempt<termios::Termios2>,
    pub pty_number: AtomicU32,
    pub pty_locked: AtomicBool,
    pub line_discipline: AtomicU32,
    /// Invalidates OFD-owned noncanonical read timers after any termios
    /// publication.  Timers never survive a semantic mode change.
    pub termios_epoch: AtomicU32,
    pub termios_waiters: PollSet,
}
impl Default for Terminal {
    fn default() -> Self {
        Self {
            lifecycle: SpinNoIrq::new(()),
            job_control: job::JobControl::new(),
            window_size: SpinNoPreempt::new(WindowSize {
                ws_row: 28,
                ws_col: 110,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            termios: SpinNoPreempt::new(termios::Termios2::default()),
            pty_number: AtomicU32::new(0),
            pty_locked: AtomicBool::new(true),
            line_discipline: AtomicU32::new(0),
            termios_epoch: AtomicU32::new(0),
            termios_waiters: PollSet::new(),
        }
    }
}
impl Terminal {
    /// Commit the new geometry before notifying its foreground group. This
    /// lock must not be held across signal delivery or a signal handler's query.
    pub fn update_window_size(&self, next: WindowSize) -> bool {
        let mut current = self.window_size.lock();
        if *current == next {
            return false;
        }
        *current = next;
        true
    }

    pub fn load_termios(&self) -> termios::Termios2 {
        *self.termios.lock()
    }

    pub fn termios_snapshot(&self) -> (u32, termios::Termios2) {
        loop {
            let before = self
                .termios_epoch
                .load(core::sync::atomic::Ordering::Acquire);
            let termios = *self.termios.lock();
            let after = self
                .termios_epoch
                .load(core::sync::atomic::Ordering::Acquire);
            if before == after {
                return (after, termios);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsize_changes_include_pixel_geometry_but_identical_updates_are_noops() {
        let terminal = Terminal::default();
        let original = *terminal.window_size.lock();
        assert!(!terminal.update_window_size(original));
        let mut next = original;
        next.ws_col += 1;
        assert!(terminal.update_window_size(next));
        assert_eq!(*terminal.window_size.lock(), next);
        assert!(!terminal.update_window_size(next));
        next.ws_xpixel = 640;
        assert!(terminal.update_window_size(next));
        assert_eq!(*terminal.window_size.lock(), next);
    }
}
