mod ntty;
mod ptm;
mod pts;
mod pty;
mod terminal;

use alloc::sync::{Arc, Weak};
use core::{any::Any, ops::Deref, sync::atomic::Ordering, task::Context};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::NodeFlags;
use axpoll::{IoEvents, PollSet, Pollable};
use axsync::Mutex;
use axtask::current;
use kspin::SpinNoIrq;
use spin::Once;
use starry_signal::{SignalInfo, Signo};
use starry_vm::{VmMutPtr, VmPtr};

pub use self::{
    ntty::{N_TTY, NTtyDriver},
    ptm::Ptmx,
    pts::PtsDir,
    pty::PtyDriver,
};
use self::{
    pts::PtsLease,
    pty::PtyEndpoint,
    terminal::{
        Terminal, WindowSize,
        ldisc::{LineDiscipline, TtyConfig, TtyRead, TtyWrite},
        termios::{Termio, Termios, Termios2},
    },
};
use crate::{
    pseudofs::DeviceOps,
    task::{AsThread, Process, Session, get_process_group, send_signal_to_process_group},
};

const N_TTY_LDISC: i32 = 0;

/// Tty device
pub struct Tty<R, W> {
    this: Once<Weak<Self>>,
    terminal: Arc<Terminal>,
    ldisc: Mutex<LineDiscipline<R, W>>,
    read_waiters: Option<Arc<PollSet>>,
    writer: W,
    endpoint: Option<PtyEndpoint>,
    pts_lease: SpinNoIrq<Option<PtsLease>>,
    is_ptm: bool,
}

impl<R: TtyRead, W: TtyWrite + Clone> Tty<R, W> {
    fn try_new(
        terminal: Arc<Terminal>,
        config: TtyConfig<R, W>,
        endpoint: Option<PtyEndpoint>,
    ) -> AxResult<Arc<Self>> {
        let writer = config.writer.clone();
        let is_ptm = endpoint.as_ref().is_some_and(PtyEndpoint::is_master);
        let ldisc = LineDiscipline::try_new(terminal.clone(), config)?;
        let read_waiters = ldisc.readiness_source().cloned();
        let ldisc = Mutex::new(ldisc);
        let tty = Arc::try_new(Self {
            this: Once::new(),
            terminal,
            ldisc,
            read_waiters,
            writer,
            endpoint,
            pts_lease: SpinNoIrq::new(None),
            is_ptm,
        })
        .map_err(|_| AxError::NoMemory)?;
        tty.this.call_once(|| Arc::downgrade(&tty));
        Ok(tty)
    }
}

impl<R: TtyRead, W: TtyWrite> Tty<R, W> {
    fn this_arc(&self) -> AxResult<Arc<Self>> {
        self.this
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::NotATty)
    }

    fn install_pts_lease(&self, lease: PtsLease) -> AxResult<()> {
        let rejected = {
            let mut current = self.pts_lease.lock();
            if current.is_some() {
                Some(lease)
            } else {
                *current = Some(lease);
                None
            }
        };
        if let Some(rejected) = rejected {
            drop(rejected);
            Err(AxError::BadState)
        } else {
            Ok(())
        }
    }

    fn release_pts_lease(&self) {
        let lease = self.pts_lease.lock().take();
        drop(lease);
    }

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
        let tty: Arc<dyn Any + Send + Sync> = self.this_arc()?;
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

    fn hangup_controlling_session(&self) {
        let Some(session) = self.terminal.job_control.session() else {
            return;
        };
        if let Some(tty) = session.terminal()
            && tty
                .downcast_ref::<Self>()
                .is_some_and(|other| Arc::ptr_eq(&other.terminal, &self.terminal))
        {
            session.unset_terminal(&tty);
        }
        if let Some(foreground) = self.terminal.job_control.release_session(&session) {
            let pgid = foreground.pgid();
            let _ = send_signal_to_process_group(pgid, Some(SignalInfo::new_kernel(Signo::SIGHUP)));
            let _ =
                send_signal_to_process_group(pgid, Some(SignalInfo::new_kernel(Signo::SIGCONT)));
        }
    }
}

pub(crate) struct PtyOpenGuard {
    // Keep the endpoint object alive until deferred final-OFD cleanup. The
    // underlying File may already have been dropped by then.
    tty: Arc<PtyDriver>,
    endpoint: PtyEndpoint,
}

impl Drop for PtyOpenGuard {
    fn drop(&mut self) {
        let master_final = self.endpoint.close();
        self.tty.writer.wake_waiters();
        if master_final {
            self.tty.release_pts_lease();
            self.tty.hangup_controlling_session();
        }
    }
}

impl Tty<pty::PtyReader, pty::PtyWriter> {
    pub(crate) fn open_description(&self) -> AxResult<PtyOpenGuard> {
        let endpoint = self.endpoint.clone().ok_or(AxError::BadState)?;
        let tty = self
            .this
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::BadState)?;
        endpoint.open()?;
        Ok(PtyOpenGuard { tty, endpoint })
    }
}

impl<R: TtyRead, W: TtyWrite> DeviceOps for Tty<R, W> {
    fn read_at(&self, buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        let slave_hangup = self
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| !endpoint.is_master() && endpoint.read_hangup());
        if slave_hangup {
            // A master hangup does not discard bytes which were already
            // accepted into the raw channel or any line-discipline stage.
            // EOF becomes visible only after the worker confirms every stage,
            // including the public ring, has drained.
            let mut ldisc = self.ldisc.lock();
            return match ldisc.read(buf) {
                Err(AxError::WouldBlock) if ldisc.input_drained() => Ok(0),
                result => result,
            };
        }
        if self.is_ptm || self.terminal.job_control.current_in_foreground() {
            let result = self.ldisc.lock().read(buf);
            if matches!(result, Err(AxError::WouldBlock))
                && self
                    .endpoint
                    .as_ref()
                    .is_some_and(|endpoint| endpoint.is_master() && endpoint.read_hangup())
            {
                Err(AxError::Io)
            } else {
                result
            }
        } else {
            Err(AxError::WouldBlock)
        }
    }

    fn write_at(&self, buf: &[u8], _offset: u64) -> AxResult<usize> {
        self.writer.write(buf)
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
                let termios = *self.terminal.termios.lock();
                (arg as *mut Termios).vm_write(*termios.deref())?;
            }
            TCGETS2 => {
                (arg as *mut Termios2).vm_write(*self.terminal.termios.lock())?;
            }
            TCSETA => {
                let termio = (arg as *const Termio).vm_read()?;
                let mut current = self.terminal.termios.lock();
                let next = Termios2::from_termio(termio, &current);
                next.validate_update(&current)?;
                *current = next;
            }
            TCSETAF | TCSETAW => return Err(AxError::Unsupported),
            TCSETS => {
                let termios = (arg as *const Termios).vm_read()?;
                let mut current = self.terminal.termios.lock();
                let next = Termios2::from_termios(termios, &current);
                next.validate_update(&current)?;
                *current = next;
            }
            TCSETSF | TCSETSW => return Err(AxError::Unsupported),
            TCSETS2 => {
                let next = (arg as *const Termios2).vm_read()?;
                let mut current = self.terminal.termios.lock();
                next.validate_update(&current)?;
                *current = next;
            }
            TCSETSF2 | TCSETSW2 => return Err(AxError::Unsupported),
            FIONREAD => {
                let readable = self.ldisc.lock().readable_len() as u32;
                (arg as *mut u32).vm_write(readable)?;
            }
            TIOCOUTQ => return Err(AxError::Unsupported),
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
                TCIFLUSH | TCOFLUSH | TCIOFLUSH => return Err(AxError::Unsupported),
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
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                (arg as *mut u32).vm_write(self.pty_number())?;
            }
            TIOCSCTTY => {
                if arg != 0 {
                    return Err(AxError::OperationNotSupported);
                }
                self.this_arc()?
                    .bind_to(&current().as_thread().proc_data.proc)?;
            }
            TIOCNOTTY => {
                let curr = current();
                let proc = &curr.as_thread().proc_data.proc;
                let session = proc.group().session();
                let tty: Arc<dyn Any + Send + Sync> = self.this_arc()?;
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
        NodeFlags::NON_CACHEABLE
            | NodeFlags::STREAM
            | NodeFlags::NO_POSITIONED_READ
            | NodeFlags::NO_POSITIONED_WRITE
            | NodeFlags::NO_SEEK
    }
}

impl<R: TtyRead, W: TtyWrite> Pollable for Tty<R, W> {
    fn poll(&self) -> IoEvents {
        let hangup_events = self
            .endpoint
            .as_ref()
            .map_or(IoEvents::empty(), PtyEndpoint::hangup_events);
        let (mut events, foreground) = if self.is_ptm {
            // The master endpoint is never subject to slave foreground job
            // control and must remain pollable without a current user task.
            (IoEvents::empty(), true)
        } else if hangup_events.contains(IoEvents::HANGUP) {
            // Hangup readiness is terminal state, independent of which task
            // happens to poll the orphaned slave.
            (IoEvents::empty(), true)
        } else {
            let events = self.terminal.job_control.poll();
            let foreground = events.contains(IoEvents::READABLE);
            (events, foreground)
        };
        events.set(IoEvents::WRITABLE, self.writer.poll_write());
        if foreground {
            events.set(IoEvents::READABLE, self.ldisc.lock().poll_read());
        }
        events |= hangup_events;
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        let job = !self.is_ptm && events.contains(IoEvents::READABLE);
        let read = events.contains(IoEvents::READABLE) && self.read_waiters.is_some();
        let write_source = events
            .contains(IoEvents::WRITABLE)
            .then(|| self.writer.tx_poll_source())
            .flatten();
        let endpoint = self.endpoint.as_ref();

        if events.contains(IoEvents::READABLE) && self.read_waiters.is_none() {
            // A manually polled transport has no honest wake source. Reject
            // blocking registration instead of manufacturing a busy loop.
            return Err(axpoll::PollRegistrationError::InvalidState);
        }

        let mut prepared = axpoll::PreparedPollRegistration::try_new(
            job as usize
                + read as usize
                + write_source.is_some() as usize
                + endpoint.is_some() as usize,
        )?;
        if job {
            prepared.arm(self.terminal.job_control.poll_source(), context.waker())?;
        }
        if let Some(source) = self.read_waiters.as_deref().filter(|_| read) {
            prepared.arm(source, context.waker())?;
        }
        if let Some(source) = write_source {
            prepared.arm(source, context.waker())?;
        }
        if let Some(endpoint) = endpoint {
            prepared.arm(endpoint.poll_source(), context.waker())?;
        }
        prepared.commit()
    }
}

impl<R, W> Drop for Tty<R, W> {
    fn drop(&mut self) {
        // Construction can publish a devpts slot before the master OFD is
        // admitted. This fallback rolls the slot back if a later allocation
        // fails; the normal path takes it from `PtyOpenGuard::drop`.
        let lease = self.pts_lease.lock().take();
        drop(lease);
    }
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, boxed::Box, sync::Arc};
    use core::task::Context;

    use axpoll::{IoEvents, Pollable};

    use super::*;
    use crate::file::{
        DescriptionResource, FileLike, Kstat, drain_deferred_description_resource_only_for_test,
        has_deferred_description_cleanup_work, prepare_file_description_with_resource,
    };

    struct DummyFile;

    impl Pollable for DummyFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
    }

    impl FileLike for DummyFile {
        fn stat(&self) -> AxResult<Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("pty-lifecycle-test"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    fn drain_all_description_cleanup() {
        drain_deferred_description_resource_only_for_test();
        assert!(!has_deferred_description_cleanup_work());
    }

    #[test]
    fn master_dup_releases_devpts_and_hangs_up_slave_only_after_deferred_final_close() {
        drain_all_description_cleanup();
        let (master, slave) = pty::create_pty_pair_for_test().unwrap();
        let (lease, slot) = pts::reserve_test_lease().unwrap();
        master.install_pts_lease(lease).unwrap();
        assert!(pts::test_slot_reserved(slot));

        let master_open = master.open_description().unwrap();
        let slave_open = slave.open_description().unwrap();

        // The final slave OFD close, rather than a duplicated fd close, owns
        // the master-side hangup transition.
        drop(slave_open);
        assert_eq!(
            master.endpoint.as_ref().unwrap().hangup_events().bits(),
            (IoEvents::READABLE | IoEvents::HANGUP).bits()
        );
        let slave_open = slave.open_description().unwrap();
        assert!(master.endpoint.as_ref().unwrap().hangup_events().is_empty());

        let resource = Box::try_new(master_open).unwrap() as DescriptionResource;
        let file: Arc<dyn FileLike> = Arc::try_new(DummyFile).unwrap();
        let description =
            prepare_file_description_with_resource(file, 0, None, Some(resource)).unwrap();
        description.mark_open_committed();

        let duplicated = description.clone();
        drop(description);
        assert!(pts::test_slot_reserved(slot));
        assert!(slave.endpoint.as_ref().unwrap().hangup_events().is_empty());

        drop(duplicated);
        // Final Arc drop only publishes preallocated work. The PTY guard, PTS
        // lease, hangup, and any worker join remain deferred.
        assert!(pts::test_slot_reserved(slot));
        assert!(slave.endpoint.as_ref().unwrap().hangup_events().is_empty());

        drain_all_description_cleanup();
        assert!(!pts::test_slot_reserved(slot));
        let events = slave.endpoint.as_ref().unwrap().hangup_events();
        assert!(events.contains(
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR | IoEvents::HANGUP
        ));
        assert_eq!(
            TtyWrite::write(&slave.writer, b"after-hangup"),
            Err(AxError::Io)
        );

        // The opened slave description remains valid as an object until its
        // own final close even though its devpts name has been unlinked.
        drop(slave_open);
        drop(slave);
        drop(master);
        drain_all_description_cleanup();
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
