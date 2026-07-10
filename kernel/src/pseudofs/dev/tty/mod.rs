mod ntty;
mod ptm;
mod pts;
mod pty;
mod terminal;

use alloc::sync::{Arc, Weak};
use core::{any::Any, ops::Deref, sync::atomic::Ordering, task::Context};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::NodeFlags;
use axpoll::{IoEvents, Pollable};
use axsync::Mutex;
use axtask::current;
use starry_process::{Process, Session};
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

use self::terminal::{
    Terminal, WindowSize,
    ldisc::{LineDiscipline, ProcessMode, TtyConfig, TtyRead, TtyWrite},
    termios::{Termio, Termios, Termios2},
};
pub use self::{
    ntty::{N_TTY, NTtyDriver},
    ptm::Ptmx,
    pts::PtsDir,
    pty::PtyDriver,
};
use crate::{
    pseudofs::{DeviceOps, SimpleFs},
    task::{AsThread, get_process_group, send_signal_to_process_group},
};

const N_TTY_LDISC: i32 = 0;

pub fn create_pty_master(fs: Arc<SimpleFs>) -> AxResult<Arc<PtyDriver>> {
    let (master, slave) = pty::create_pty_pair();
    pts::add_slave(fs, slave)?;
    Ok(master)
}

/// Tty device
pub struct Tty<R, W> {
    this: Weak<Self>,
    terminal: Arc<Terminal>,
    ldisc: Mutex<LineDiscipline<R, W>>,
    writer: W,
    is_ptm: bool,
}

impl<R: TtyRead, W: TtyWrite + Clone> Tty<R, W> {
    fn new(terminal: Arc<Terminal>, config: TtyConfig<R, W>) -> Arc<Self> {
        let writer = config.writer.clone();
        let is_ptm = matches!(&config.process_mode, ProcessMode::None(_));
        let ldisc = Mutex::new(LineDiscipline::new(terminal.clone(), config));
        Arc::new_cyclic(|this| Self {
            this: this.clone(),
            terminal,
            ldisc,
            writer,
            is_ptm,
        })
    }
}

impl<R: TtyRead, W: TtyWrite> Tty<R, W> {
    fn controlling_session_for_current(&self) -> AxResult<Arc<Session>> {
        let session = current().as_thread().proc_data.proc.group().session();
        let terminal_session = self
            .terminal
            .job_control
            .session()
            .ok_or(AxError::NotATty)?;
        if !Arc::ptr_eq(&session, &terminal_session) {
            return Err(AxError::NotATty);
        }
        let tty: Arc<dyn Any + Send + Sync> = self.this.upgrade().ok_or(AxError::NotATty)?;
        if !session
            .terminal()
            .is_some_and(|current| Arc::ptr_eq(&current, &tty))
        {
            return Err(AxError::NotATty);
        }
        Ok(session)
    }

    pub fn bind_to(self: &Arc<Self>, proc: &Process) -> AxResult<()> {
        let pg = proc.group();
        let session = pg.session();
        if session.sid() != proc.pid() {
            return Err(AxError::OperationNotPermitted);
        }

        let tty: Arc<dyn Any + Send + Sync> = self.clone();
        if let Some(current) = session.terminal() {
            if !Arc::ptr_eq(&current, &tty) {
                return Err(AxError::OperationNotPermitted);
            }
            self.terminal.job_control.claim_session(&session)?;
            return self.terminal.job_control.set_foreground(&pg);
        }

        let claimed = self.terminal.job_control.claim_session(&session)?;
        if !session.set_terminal_with(|| tty.clone()) {
            if claimed {
                self.terminal.job_control.release_session(&session);
            }
            return Err(AxError::OperationNotPermitted);
        }
        if let Err(err) = self.terminal.job_control.set_foreground(&pg) {
            session.unset_terminal(&tty);
            if claimed {
                self.terminal.job_control.release_session(&session);
            }
            return Err(err);
        }
        Ok(())
    }

    pub fn pty_number(&self) -> u32 {
        self.terminal.pty_number.load(Ordering::Acquire)
    }

    pub fn is_locked_pty_slave(&self) -> bool {
        !self.is_ptm && self.terminal.pty_locked.load(Ordering::Acquire)
    }
}

impl<R: TtyRead, W: TtyWrite> DeviceOps for Tty<R, W> {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        if self.is_ptm || self.terminal.job_control.current_in_foreground() {
            self.ldisc.lock().read(buf)
        } else {
            Err(AxError::WouldBlock)
        }
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> AxResult<usize> {
        self.writer.write(buf);
        Ok(buf.len())
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> AxResult<usize> {
        use linux_raw_sys::{
            general::{CAP_SYS_ADMIN, TCIFLUSH, TCIOFF, TCIOFLUSH, TCION, TCOFLUSH, TCOOFF, TCOON},
            ioctl::*,
        };
        match cmd {
            TCGETA => {
                (arg as *mut Termio).vm_write(self.terminal.termios.lock().as_termio())?;
            }
            TCGETS => {
                (arg as *mut Termios).vm_write(*self.terminal.termios.lock().as_ref().deref())?;
            }
            TCGETS2 => {
                (arg as *mut Termios2).vm_write(*self.terminal.termios.lock().as_ref())?;
            }
            TCSETA | TCSETAF | TCSETAW => {
                let termio = (arg as *const Termio).vm_read()?;
                let current = self.terminal.termios.lock();
                let next = Termios2::from_termio(termio, current.as_ref());
                drop(current);
                *self.terminal.termios.lock() = Arc::new(next);
                if cmd == TCSETAF {
                    self.ldisc.lock().drain_input();
                }
            }
            TCSETS | TCSETSF | TCSETSW => {
                // TODO: drain output?
                *self.terminal.termios.lock() =
                    Arc::new(Termios2::new((arg as *const Termios).vm_read()?));
                if cmd == TCSETSF {
                    self.ldisc.lock().drain_input();
                }
            }
            TCSETS2 | TCSETSF2 | TCSETSW2 => {
                // TODO: drain output?
                *self.terminal.termios.lock() = Arc::new((arg as *const Termios2).vm_read()?);
                if cmd == TCSETSF2 {
                    self.ldisc.lock().drain_input();
                }
            }
            FIONREAD => {
                let readable = self.ldisc.lock().readable_len() as u32;
                (arg as *mut u32).vm_write(readable)?;
            }
            TIOCOUTQ => {
                (arg as *mut u32).vm_write(0)?;
            }
            TIOCGETD => {
                let ldisc = self.terminal.line_discipline.load(Ordering::Acquire) as i32;
                (arg as *mut i32).vm_write(ldisc)?;
            }
            TIOCSETD => {
                let ldisc = (arg as *const i32).vm_read()?;
                if ldisc != N_TTY_LDISC {
                    return Err(AxError::InvalidInput);
                }
                self.terminal
                    .line_discipline
                    .store(ldisc as u32, Ordering::Release);
            }
            TCXONC => match arg as u32 {
                TCOOFF | TCOON | TCIOFF | TCION => return Err(AxError::Unsupported),
                _ => return Err(AxError::InvalidInput),
            },
            TCFLSH => match arg as u32 {
                TCIFLUSH => self.ldisc.lock().drain_input(),
                TCOFLUSH | TCIOFLUSH => return Err(AxError::Unsupported),
                _ => return Err(AxError::InvalidInput),
            },
            TIOCGPGRP => {
                self.controlling_session_for_current()?;
                let foreground = self
                    .terminal
                    .job_control
                    .foreground()
                    .ok_or(AxError::NoSuchProcess)?;
                (arg as *mut u32).vm_write(foreground.pgid())?;
            }
            TIOCSPGRP => {
                self.controlling_session_for_current()?;
                let pgid = (arg as *const i32).vm_read()?;
                if pgid <= 0 {
                    return Err(AxError::InvalidInput);
                }
                let foreground = get_process_group(pgid as u32)?;
                self.terminal.job_control.set_foreground(&foreground)?;
            }
            TIOCGWINSZ => {
                (arg as *mut WindowSize).vm_write(*self.terminal.window_size.lock())?;
            }
            TIOCSWINSZ => {
                *self.terminal.window_size.lock() = (arg as *const WindowSize).vm_read()?;
            }
            TIOCSPTLCK => {
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                let locked = (arg as *const i32).vm_read()? != 0;
                self.terminal.pty_locked.store(locked, Ordering::Release);
            }
            TIOCGPTLCK => {
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                let locked = self.terminal.pty_locked.load(Ordering::Acquire) as i32;
                (arg as *mut i32).vm_write(locked)?;
            }
            TIOCGPTN => {
                (arg as *mut u32).vm_write(self.pty_number())?;
            }
            TIOCSCTTY => {
                if arg != 0 {
                    return Err(AxError::OperationNotSupported);
                }
                self.this
                    .upgrade()
                    .ok_or(AxError::NotATty)?
                    .bind_to(&current().as_thread().proc_data.proc)?;
            }
            TIOCNOTTY => {
                let curr = current();
                let proc = &curr.as_thread().proc_data.proc;
                let session = proc.group().session();
                let tty: Arc<dyn Any + Send + Sync> =
                    self.this.upgrade().ok_or(AxError::NotATty)?;
                if !session
                    .terminal()
                    .is_some_and(|current| Arc::ptr_eq(&current, &tty))
                {
                    return Err(AxError::NotATty);
                }
                if session.sid() != proc.pid() {
                    // The process model currently stores the controlling tty on
                    // the session, so it cannot represent Linux's per-process
                    // non-leader detach without disconnecting the whole session.
                    return Err(AxError::OperationNotSupported);
                }

                let foreground = self.terminal.job_control.foreground();
                if !session.unset_terminal(&tty) {
                    return Err(AxError::NotATty);
                }
                self.terminal.job_control.release_session(&session);
                if let Some(foreground) = foreground {
                    let pgid = foreground.pgid();
                    let _ = send_signal_to_process_group(
                        pgid,
                        Some(SignalInfo::new_kernel(Signo::SIGHUP)),
                    );
                    let _ = send_signal_to_process_group(
                        pgid,
                        Some(SignalInfo::new_kernel(Signo::SIGCONT)),
                    );
                }
            }
            TIOCGSID => {
                let session = self
                    .terminal
                    .job_control
                    .session()
                    .ok_or(AxError::NotATty)?;
                (arg as *mut u32).vm_write(session.sid())?;
            }
            TIOCVHANGUP => {
                if !current()
                    .as_thread()
                    .proc_data
                    .has_effective_capability(CAP_SYS_ADMIN)
                {
                    return Err(AxError::OperationNotPermitted);
                }
                return Err(AxError::Unsupported);
            }
            _ => return Err(AxError::NotATty),
        }
        Ok(0)
    }

    fn as_pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }

    /// Casts the device operations to a dynamic type.
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM
    }
}

impl<R: TtyRead, W: TtyWrite> Pollable for Tty<R, W> {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT | self.terminal.job_control.poll();
        if self.is_ptm || events.contains(IoEvents::IN) {
            events.set(IoEvents::IN, self.ldisc.lock().poll_read());
        }
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if !self.is_ptm {
            self.terminal.job_control.register(context, events);
        }
        if events.contains(IoEvents::IN) {
            self.ldisc.lock().register_rx_waker(context.waker());
        }
    }
}

pub struct CurrentTty;
impl DeviceOps for CurrentTty {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn ioctl(&self, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
