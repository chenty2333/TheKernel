pub(crate) mod fbcon;
mod ntty;
mod ptm;
mod pts;
mod pty;
mod seat;
mod terminal;
mod vt;

use alloc::{
    borrow::Cow,
    boxed::Box,
    string::String,
    sync::{Arc, Weak},
};
use core::{
    any::Any,
    mem::{MaybeUninit, align_of, offset_of, size_of},
    ops::Deref,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    task::Context,
    time::Duration,
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{Location, NodeFlags, VfsResult};
use axio::prelude::*;
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use axsync::Mutex;
use kspin::SpinNoIrq;
use spin::Once;
use thekernel_linux_signal::{SignalInfo, Signo};

pub(crate) use self::seat::{remember_input_node, remember_primary_node};
pub use self::{
    ntty::{N_TTY, NTtyDriver},
    ptm::Ptmx,
    pts::PtsDir,
    pty::PtyDriver,
    vt::{VT_MANAGER, VtDevice, notify_vt_owner_exit},
};
use self::{
    pts::PtsLease,
    pty::PtyEndpoint,
    terminal::{
        Terminal, WindowSize,
        job::SessionRelease,
        ldisc::{LineDiscipline, TtyConfig, TtyRead, TtyWrite},
        termios::{Termio, Termios, Termios2},
    },
};
use crate::{
    file::{DescriptionResource, FileLike, IoDst, IoSrc, IoctlContext, Kstat, OfdIoStatus},
    mm::map_usercopy_error,
    pseudofs::{DeviceOpen, DeviceOps},
    task::{AsThread, Process, Session, get_process_group, send_signal_to_process_group},
};

const N_TTY_LDISC: i32 = 0;

const _: () = assert!(size_of::<WindowSize>() == 8 && align_of::<WindowSize>() == 2);

fn window_size_to_user_bytes(window_size: WindowSize) -> [u8; size_of::<WindowSize>()] {
    let mut bytes = [0u8; size_of::<WindowSize>()];
    bytes[offset_of!(WindowSize, ws_row)..][..2].copy_from_slice(&window_size.ws_row.to_ne_bytes());
    bytes[offset_of!(WindowSize, ws_col)..][..2].copy_from_slice(&window_size.ws_col.to_ne_bytes());
    bytes[offset_of!(WindowSize, ws_xpixel)..][..2]
        .copy_from_slice(&window_size.ws_xpixel.to_ne_bytes());
    bytes[offset_of!(WindowSize, ws_ypixel)..][..2]
        .copy_from_slice(&window_size.ws_ypixel.to_ne_bytes());
    bytes
}

fn termios_user_bytes(termios: &Termios2) -> [u8; size_of::<Termios>()] {
    termios.deref().to_user_bytes()
}

fn termios2_user_bytes(termios: &Termios2) -> [u8; size_of::<Termios2>()] {
    termios.to_user_bytes()
}

fn read_user_bytes<const N: usize>(context: &IoctlContext, address: usize) -> AxResult<[u8; N]> {
    let mut bytes = [MaybeUninit::<u8>::uninit(); N];
    context
        .user_memory()
        .read_bytes(address, &mut bytes)
        .map_err(map_usercopy_error)?;
    Ok(core::array::from_fn(|index| {
        // SAFETY: read_bytes initializes every byte before returning.
        unsafe { bytes[index].assume_init() }
    }))
}

fn window_size_from_user_bytes(bytes: [u8; size_of::<WindowSize>()]) -> WindowSize {
    let read_u16 = |offset| u16::from_ne_bytes(bytes[offset..][..2].try_into().unwrap());
    WindowSize {
        ws_row: read_u16(offset_of!(WindowSize, ws_row)),
        ws_col: read_u16(offset_of!(WindowSize, ws_col)),
        ws_xpixel: read_u16(offset_of!(WindowSize, ws_xpixel)),
        ws_ypixel: read_u16(offset_of!(WindowSize, ws_ypixel)),
    }
}

/// Tty device
pub struct Tty<R, W> {
    this: Once<Weak<Self>>,
    terminal: Arc<Terminal>,
    ldisc: Mutex<LineDiscipline<R, W>>,
    read_waiters: Option<Arc<PollSet>>,
    writer: W,
    hung_up: core::sync::atomic::AtomicBool,
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
            hung_up: core::sync::atomic::AtomicBool::new(false),
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
    /// Routes bytes from a uniquely-owned transport into this terminal's
    /// manual line discipline. Virtual consoles use this instead of letting
    /// every VT race to read the hardware console.
    pub(crate) fn inject_input(&self, bytes: &[u8]) -> AxResult<()> {
        self.ldisc.lock().inject_input(bytes)
    }

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

    fn controlling_session(&self, session: &Arc<Session>) -> AxResult<Arc<Session>> {
        let terminal_session = self
            .terminal
            .job_control
            .session()
            .ok_or(AxError::NotATty)?;
        if !Arc::ptr_eq(session, &terminal_session) {
            return Err(AxError::NotATty);
        }
        let tty: Arc<dyn Any + Send + Sync> = self.this_arc()?;
        if !session
            .terminal()
            .is_some_and(|current| Arc::ptr_eq(&current, &tty))
        {
            return Err(AxError::NotATty);
        }
        Ok(session.clone())
    }

    pub fn bind_to(self: &Arc<Self>, proc: &Process) -> AxResult<()> {
        let _lifecycle = self.terminal.lifecycle.lock();
        self.ensure_bindable()?;
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

    /// Rejects a new controlling-session claim after a synchronous hangup.
    fn ensure_bindable(&self) -> AxResult<()> {
        if self.hung_up.load(Ordering::Acquire) {
            Err(AxError::Io)
        } else {
            Ok(())
        }
    }

    pub fn pty_number(&self) -> u32 {
        self.terminal.pty_number.load(Ordering::Acquire)
    }

    pub fn is_locked_pty_slave(&self) -> bool {
        !self.is_ptm && self.terminal.pty_locked.load(Ordering::Acquire)
    }

    /// Implements the drain/flush ordering shared by all termios setters.
    /// User data has already been copied before entering here; the update is
    /// therefore never visible until a requested drain completed.
    fn set_termios_after_output(
        &self,
        next: Termios2,
        drain: bool,
        flush_input: bool,
    ) -> AxResult<()> {
        {
            let current = self.terminal.termios.lock();
            next.validate_update(&current)?;
        }
        if drain {
            if let Some(source) = self.writer.output_poll_source() {
                crate::readiness::block_on_poll_set(source, || {
                    if self.hung_up.load(Ordering::Acquire) {
                        return Err(AxError::Io);
                    }
                    if self.writer.output_pending() == 0 && self.ldisc.lock().output_pending() == 0
                    {
                        Ok(())
                    } else {
                        Err(AxError::WouldBlock)
                    }
                })?;
            } else if self.writer.output_pending() != 0 {
                return Err(AxError::WouldBlock);
            }
        }
        if flush_input {
            self.ldisc.lock().flush_input()?;
        }
        {
            let mut current = self.terminal.termios.lock();
            next.validate_update(&current)?;
            *current = next;
            // Readers take epoch/termios/epoch snapshots; keep the version
            // publication inside this lock so no new termios image is ever
            // observable with the old epoch.
            self.terminal.termios_epoch.fetch_add(1, Ordering::AcqRel);
        }
        self.terminal.termios_waiters.wake();
        Ok(())
    }

    /// Whether this tty is currently claimed as a controlling terminal.
    ///
    /// This is deliberately an association query, rather than an FD-count
    /// query: a session may retain its ctty after every file description has
    /// been closed.
    pub(crate) fn has_controlling_session(&self) -> bool {
        self.terminal.job_control.session().is_some()
    }

    /// Synchronously tears down this terminal's controlling-session state.
    ///
    /// The terminal job-control association is the synchronization point for
    /// both a PTY master's final close and `vhangup(2)`: only the transition
    /// which retires that association owns foreground signal delivery.
    fn hangup_controlling_session(&self) {
        let Some(session) = self.terminal.job_control.session() else {
            return;
        };
        self.hangup_session(&session);
    }

    /// Hangs up this terminal only when it is still controlled by `expected`.
    ///
    /// A vhangup caller first snapshots its session terminal.  The identity
    /// check prevents that stale snapshot from tearing down a later session
    /// which claimed the same terminal after a concurrent last close.
    fn hangup_session(&self, expected: &Arc<Session>) {
        let (released, target) = {
            let _lifecycle = self.terminal.lifecycle.lock();
            let Some(session) = self.terminal.job_control.session() else {
                return;
            };
            if !Arc::ptr_eq(&session, expected) {
                return;
            }
            let Some(target) = session.terminal() else {
                return;
            };
            if !target
                .downcast_ref::<Self>()
                .is_some_and(|tty| Arc::ptr_eq(&tty.terminal, &self.terminal))
            {
                return;
            }
            // Publish the one-way admission barrier before retiring the
            // association. A waiter which reaches bind_to after this lock is
            // released must reject instead of claiming a tty whose ldisc is
            // about to be cancelled outside this critical section.
            target
                .downcast_ref::<Self>()
                .expect("validated controlling tty type")
                .hung_up
                .store(true, Ordering::Release);
            let released = self.terminal.job_control.release_session(&session);
            let SessionRelease::Released(foreground) = released else {
                return;
            };
            session.unset_terminal(&target);
            (foreground, target)
        };
        // On a PTY master close, `self` is the master but the controlling
        // terminal is its slave.  Hang up that endpoint's operations.
        target
            .downcast_ref::<Self>()
            .expect("validated controlling tty type")
            .hangup_io();
        if let Some(foreground) = released {
            let pgid = foreground.pgid();
            let _ = send_signal_to_process_group(pgid, Some(SignalInfo::new_kernel(Signo::SIGHUP)));
            let _ =
                send_signal_to_process_group(pgid, Some(SignalInfo::new_kernel(Signo::SIGCONT)));
        }
    }

    /// Makes existing terminal file operations observe a synchronous hangup.
    fn hangup_io(&self) {
        self.hung_up.store(true, Ordering::Release);
        self.ldisc.lock().hangup();
        if let Some(waiters) = &self.read_waiters {
            waiters.wake();
        }
        self.writer.wake_waiters();
        if let Some(endpoint) = &self.endpoint {
            endpoint.wake_waiters();
        }
    }
}

/// Hangs up `session`'s controlling terminal, if it has one.
///
/// A session keeps its controlling terminal as an erased `Arc`, while the
/// terminal state machine is shared by the concrete N_TTY and PTY drivers.
/// Keep that type recovery here so syscall code neither learns driver details
/// nor duplicates the close/hangup transition.
pub(crate) fn vhangup_controlling_session(session: &Arc<Session>) {
    let Some(tty) = session.terminal() else {
        return;
    };

    if let Some(tty) = tty.downcast_ref::<NTtyDriver>() {
        tty.hangup_session(session);
    } else if let Some(tty) = tty.downcast_ref::<PtyDriver>() {
        tty.hangup_session(session);
    }
}

pub(crate) struct PtyOpenGuard<R: TtyRead, W: TtyWrite> {
    // Keep the endpoint object alive until deferred final-OFD cleanup. The
    // underlying File may already have been dropped by then.
    tty: Arc<Tty<R, W>>,
    endpoint: PtyEndpoint,
}

struct TtyReadState {
    armed: AtomicBool,
    expired: AtomicBool,
    generation: AtomicU32,
    observed_input: AtomicUsize,
    termios_epoch: AtomicU32,
    wake: Arc<PollSet>,
}

impl TtyReadState {
    fn try_new() -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            armed: AtomicBool::new(false),
            expired: AtomicBool::new(false),
            generation: AtomicU32::new(0),
            observed_input: AtomicUsize::new(0),
            termios_epoch: AtomicU32::new(u32::MAX),
            wake: Arc::try_new(PollSet::new()).map_err(|_| AxError::NoMemory)?,
        })
        .map_err(|_| AxError::NoMemory)
    }
    fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.armed.store(false, Ordering::Release);
        self.expired.store(false, Ordering::Release);
        self.observed_input.store(0, Ordering::Release);
    }
    fn take_expired(&self) -> bool {
        self.expired.swap(false, Ordering::AcqRel)
    }
    fn arm(self: &Arc<Self>, deciseconds: u8) -> AxResult<()> {
        if self.armed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let state = self.clone();
        axtask::try_spawn_with_name(
            move || {
                let _ = axtask::sleep(Duration::from_millis(u64::from(deciseconds) * 100));
                if state.generation.load(Ordering::Acquire) == generation {
                    state.armed.store(false, Ordering::Release);
                    state.expired.store(true, Ordering::Release);
                    state.wake.wake();
                }
            },
            String::from("tty-vtime"),
        )
        .map_err(|_| AxError::NoMemory)?;
        Ok(())
    }
}

impl<R: TtyRead, W: TtyWrite> Drop for PtyOpenGuard<R, W> {
    fn drop(&mut self) {
        let master_final = self.endpoint.close();
        self.tty.writer.wake_waiters();
        if master_final {
            self.tty.release_pts_lease();
            self.tty.hangup_controlling_session();
        }
    }
}

/// Per-open TTY adapter.  In particular, VMIN/VTIME state is OFD-owned:
/// dup/fork share it, independent opens never do.
pub(crate) struct TtyFile<R: TtyRead, W: TtyWrite> {
    tty: Arc<Tty<R, W>>,
    location: Location,
    nonblocking: AtomicBool,
    read_state: Arc<TtyReadState>,
    read_gate: Mutex<()>,
}

impl<R: TtyRead, W: TtyWrite> TtyFile<R, W> {
    pub(crate) fn try_new(tty: Arc<Tty<R, W>>, location: Location) -> AxResult<Arc<Self>> {
        Arc::try_new(Self {
            tty,
            location,
            nonblocking: AtomicBool::new(false),
            read_state: TtyReadState::try_new()?,
            read_gate: Mutex::new(()),
        })
        .map_err(|_| AxError::NoMemory)
    }

    fn read_once(&self, out: &mut [u8]) -> AxResult<usize> {
        let (epoch, term) = self.tty.terminal.termios_snapshot();
        if self.read_state.termios_epoch.swap(epoch, Ordering::AcqRel) != epoch {
            self.read_state.cancel();
            self.read_state
                .termios_epoch
                .store(epoch, Ordering::Release);
        }
        if term.canonical() {
            return DeviceOps::read_at(self.tty.as_ref(), out, 0);
        }
        let available = self.tty.ldisc.lock().readable_len();
        let vmin = term.special_char(linux_raw_sys::general::VMIN) as usize;
        let vtime = term.special_char(linux_raw_sys::general::VTIME);
        let threshold = vmin.min(out.len());
        match (threshold, vtime, available) {
            (0, 0, _) => {}
            (0, time, 0) => {
                if !self.read_state.take_expired() {
                    self.read_state.arm(time)?;
                    return Err(AxError::WouldBlock);
                }
                return Ok(0);
            }
            (need, 0, have) if have < need => return Err(AxError::WouldBlock),
            (need, time, have) if have < need => {
                if have == 0 {
                    return Err(AxError::WouldBlock);
                }
                // N_TTY's VMIN/VTIME timer is inter-byte: every newly
                // observed byte restarts this OFD's deadline.
                let prior = self.read_state.observed_input.swap(have, Ordering::AcqRel);
                if prior != have && self.read_state.armed.load(Ordering::Acquire) {
                    self.read_state.cancel();
                    self.read_state
                        .observed_input
                        .store(have, Ordering::Release);
                }
                if !self.read_state.take_expired() {
                    self.read_state.arm(time)?;
                    return Err(AxError::WouldBlock);
                }
            }
            _ => {}
        }
        DeviceOps::read_at(self.tty.as_ref(), out, 0)
    }

    fn read_with_nonblocking(&self, dst: &mut IoDst, nonblocking: bool) -> AxResult<usize> {
        let _read_owner = self.read_gate.lock();
        let mut bytes = [0u8; 4096];
        let length = dst.remaining_mut().min(bytes.len());
        if length == 0 {
            return Ok(0);
        }
        let result =
            crate::readiness::block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
                self.read_once(&mut bytes[..length])
            });
        self.read_state.cancel();
        let read = result?;
        dst.write_all(&bytes[..read])?;
        Ok(read)
    }

    fn write_with_nonblocking(&self, src: &mut IoSrc, nonblocking: bool) -> AxResult<usize> {
        let mut bytes = [0u8; 4096];
        let count = src.read(&mut bytes)?;
        crate::readiness::block_on_poll_io(self, IoEvents::WRITABLE, nonblocking, || {
            DeviceOps::write_at(self.tty.as_ref(), &bytes[..count], 0)
        })
    }
}

impl<R: TtyRead, W: TtyWrite> FileLike for TtyFile<R, W> {
    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking())
    }
    fn write(&self, src: &mut IoSrc) -> AxResult<usize> {
        self.write_with_nonblocking(src, self.nonblocking())
    }
    fn read_with_operation_status(&self, status: OfdIoStatus, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking() || status.rwf_nowait())
    }
    fn write_with_operation_status(&self, status: OfdIoStatus, src: &mut IoSrc) -> AxResult<usize> {
        self.write_with_nonblocking(src, self.nonblocking() || status.rwf_nowait())
    }
    fn stat(&self) -> AxResult<Kstat> {
        let metadata = self.location.metadata()?;
        Ok(Kstat {
            dev: crate::mounts::linux_device_id(metadata.device).0,
            mnt_id: self.location.mountpoint().mount_id(),
            ino: metadata.inode,
            nlink: metadata.nlink as _,
            mode: ((metadata.node_type as u8 as u32) << 12) | metadata.mode.bits() as u32,
            uid: metadata.uid,
            gid: metadata.gid,
            size: metadata.size,
            blksize: metadata.block_size as _,
            blocks: metadata.blocks,
            rdev: metadata.rdev,
            atime: metadata.atime,
            btime: metadata.btime,
            mtime: metadata.mtime,
            ctime: metadata.ctime,
            ..Kstat::default()
        })
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Owned(self.location.absolute_path()?))
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        self.tty.ioctl(context, cmd, arg)
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult<()> {
        self.nonblocking.store(value, Ordering::Release);
        Ok(())
    }
    fn final_close(&self) {
        self.read_state.cancel();
    }
}

impl<R: TtyRead, W: TtyWrite> Pollable for TtyFile<R, W> {
    fn poll(&self) -> IoEvents {
        let mut events = self.tty.poll();
        events.set(
            IoEvents::READABLE,
            events.contains(IoEvents::READABLE) || self.read_state.expired.load(Ordering::Acquire),
        );
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        let mut prepared = axpoll::PreparedPollRegistration::try_new(3)?;
        prepared.arm_owned(self.read_state.wake.clone(), context.waker())?;
        prepared.arm(&self.tty.terminal.termios_waiters, context.waker())?;
        prepared.arm_nested(|| self.tty.register(context, events))?;
        prepared.commit()
    }
}

impl<R: TtyRead, W: TtyWrite> Tty<R, W> {
    pub(crate) fn open_transport_description(&self) -> AxResult<Option<PtyOpenGuard<R, W>>> {
        let Some(endpoint) = self.endpoint.clone() else {
            return Ok(None);
        };
        let tty = self
            .this
            .get()
            .and_then(Weak::upgrade)
            .ok_or(AxError::BadState)?;
        endpoint.open()?;
        Ok(Some(PtyOpenGuard { tty, endpoint }))
    }
}

impl<R: TtyRead, W: TtyWrite> DeviceOps for Tty<R, W> {
    fn open_description(&self, location: &Location, _flags: u32) -> VfsResult<Option<DeviceOpen>> {
        let tty = self.this_arc()?;
        let file: Arc<dyn FileLike> = TtyFile::try_new(tty, location.clone())?;
        let guard = self.open_transport_description()?;
        let resource = guard.map(|guard| Box::new(guard) as DescriptionResource);
        Ok(Some(DeviceOpen::new(file, resource)))
    }

    fn read_at(&self, buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        if self.hung_up.load(Ordering::Acquire) {
            return Ok(0);
        }
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
        if self.hung_up.load(Ordering::Acquire) {
            return Err(AxError::Io);
        }
        self.writer.write(buf)
    }

    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        use linux_raw_sys::{
            general::{CAP_SYS_ADMIN, TCIFLUSH, TCIOFF, TCIOFLUSH, TCION, TCOFLUSH, TCOOFF, TCOON},
            ioctl::*,
        };
        match cmd {
            TCGETA => {
                let bytes = self.terminal.termios.lock().as_termio().to_user_bytes();
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            TCGETS => {
                let termios = *self.terminal.termios.lock();
                let bytes = termios_user_bytes(&termios);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            TCGETS2 => {
                let termios = *self.terminal.termios.lock();
                let bytes = termios2_user_bytes(&termios);
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            TCSETA | TCSETAW | TCSETAF => {
                let termio = Termio::from_user_bytes(read_user_bytes(context, arg)?);
                let current = self.terminal.load_termios();
                let next = Termios2::from_termio(termio, &current);
                self.set_termios_after_output(next, cmd != TCSETA, cmd == TCSETAF)?;
            }
            TCSETS | TCSETSW | TCSETSF => {
                let termios = Termios::from_user_bytes(read_user_bytes(context, arg)?);
                let current = self.terminal.load_termios();
                let next = Termios2::from_termios(termios, &current);
                self.set_termios_after_output(next, cmd != TCSETS, cmd == TCSETSF)?;
            }
            TCSETS2 | TCSETSW2 | TCSETSF2 => {
                let next = Termios2::from_user_bytes(read_user_bytes(context, arg)?);
                self.set_termios_after_output(next, cmd != TCSETS2, cmd == TCSETSF2)?;
            }
            FIONREAD => return Ok(self.ldisc.lock().readable_len()),
            TIOCOUTQ => {
                let pending = self
                    .writer
                    .output_pending()
                    .saturating_add(self.ldisc.lock().output_pending())
                    as i32;
                context
                    .user_memory()
                    .write_bytes(arg, &pending.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCGETD => {
                let ldisc = self.terminal.line_discipline.load(Ordering::Acquire) as i32;
                context
                    .user_memory()
                    .write_bytes(arg, &ldisc.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCSETD => {
                let ldisc = context
                    .user_memory()
                    .read_value(arg as *const i32)
                    .map_err(map_usercopy_error)?;
                if ldisc != N_TTY_LDISC {
                    return Err(AxError::InvalidInput);
                }
                self.terminal
                    .line_discipline
                    .store(ldisc as u32, Ordering::Release);
            }
            TCXONC => match arg as u32 {
                TCOOFF => self.writer.set_output_stopped(true),
                TCOON => self.writer.set_output_stopped(false),
                TCIOFF => {
                    let stop = self
                        .terminal
                        .load_termios()
                        .special_char(linux_raw_sys::general::VSTOP);
                    if stop != 0 {
                        self.writer.write(&[stop])?;
                    }
                }
                TCION => {
                    let start = self
                        .terminal
                        .load_termios()
                        .special_char(linux_raw_sys::general::VSTART);
                    if start != 0 {
                        self.writer.write(&[start])?;
                    }
                }
                _ => return Err(AxError::InvalidInput),
            },
            TCFLSH => match arg as u32 {
                TCIFLUSH => self.ldisc.lock().flush_input()?,
                TCOFLUSH => self.writer.flush_output(),
                TCIOFLUSH => {
                    self.ldisc.lock().flush_input()?;
                    self.writer.flush_output();
                }
                _ => return Err(AxError::InvalidInput),
            },
            TIOCGPGRP => {
                self.controlling_session(context.caller_session())?;
                let foreground = self
                    .terminal
                    .job_control
                    .foreground()
                    .ok_or(AxError::NoSuchProcess)?;
                // The registry keys groups by kernel-global leader identity,
                // but tcgetpgrp(3) reports the caller-namespace pgid (the
                // same translation setpgid(2) performs).
                let pid_ns = context.caller_task().as_thread().pid_ns();
                let pgid = pid_ns
                    .visible_pid_for(&pid_ns, foreground.pgid())
                    .ok_or(AxError::NoSuchProcess)?;
                context
                    .user_memory()
                    .write_bytes(arg, &pgid.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCSPGRP => {
                self.controlling_session(context.caller_session())?;
                let pgid = context
                    .user_memory()
                    .read_value(arg as *const i32)
                    .map_err(map_usercopy_error)?;
                if pgid <= 0 {
                    return Err(AxError::InvalidInput);
                }
                // The pgid arrives in the caller's pid namespace; resolve it
                // to the kernel-global leader identity before the registry
                // lookup (the same translation setpgid(2) performs).
                let pid_ns = context.caller_task().as_thread().pid_ns();
                let pgid = pid_ns
                    .resolve_visible_pid(pgid as _)
                    .ok_or(AxError::NoSuchProcess)?;
                let foreground = get_process_group(pgid)?;
                self.terminal.job_control.set_foreground(&foreground)?;
            }
            TIOCGWINSZ => {
                let bytes = window_size_to_user_bytes(*self.terminal.window_size.lock());
                context
                    .user_memory()
                    .write_bytes(arg, &bytes)
                    .map_err(map_usercopy_error)?;
            }
            TIOCSWINSZ => {
                let window_size = window_size_from_user_bytes(read_user_bytes(context, arg)?);
                *self.terminal.window_size.lock() = window_size;
            }
            TIOCSPTLCK => {
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                let locked = context
                    .user_memory()
                    .read_value(arg as *const i32)
                    .map_err(map_usercopy_error)?
                    != 0;
                self.terminal.pty_locked.store(locked, Ordering::Release);
            }
            TIOCGPTLCK => {
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                let locked = self.terminal.pty_locked.load(Ordering::Acquire) as i32;
                context
                    .user_memory()
                    .write_bytes(arg, &locked.to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCGPTN => {
                if !self.is_ptm {
                    return Err(AxError::NotATty);
                }
                context
                    .user_memory()
                    .write_bytes(arg, &self.pty_number().to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCSCTTY => {
                if arg != 0 {
                    return Err(AxError::OperationNotSupported);
                }
                self.this_arc()?.bind_to(&context.caller_process().proc)?;
            }
            TIOCNOTTY => {
                let _lifecycle = self.terminal.lifecycle.lock();
                let proc = &context.caller_process().proc;
                let session = context.caller_session().clone();
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

                if !session.unset_terminal(&tty) {
                    return Err(AxError::NotATty);
                }
                let released = self.terminal.job_control.release_session(&session);
                drop(_lifecycle);
                if let SessionRelease::Released(Some(foreground)) = released {
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
                context
                    .user_memory()
                    .write_bytes(arg, &session.sid().to_ne_bytes())
                    .map_err(map_usercopy_error)?;
            }
            TIOCVHANGUP => {
                if !context
                    .caller_cred()
                    .has_effective_capability(CAP_SYS_ADMIN)
                {
                    return Err(AxError::OperationNotPermitted);
                }
                self.hangup_controlling_session();
                // vhangup marks the target terminal dead even when it has no
                // controlling session; session signal delivery is merely the
                // conditional second half of the operation above.
                self.hangup_io();
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
        if self.hung_up.load(Ordering::Acquire) {
            return IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR | IoEvents::HANGUP;
        }
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

pub struct CurrentTty;
impl DeviceOps for CurrentTty {
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn write_at(&self, _buf: &[u8], _offset: u64) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn ioctl(&self, _context: &IoctlContext, _cmd: u32, _arg: usize) -> AxResult<usize> {
        Err(AxError::NotATty)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{borrow::Cow, boxed::Box, sync::Arc};
    use core::task::Context;
    use std::{sync::mpsc, thread};

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

        fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
            Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
                b"pty-lifecycle-test",
            )))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    #[test]
    fn termios_ioctl_codecs_use_linux_abi_sizes() {
        let termios = Termios2::default();

        assert_eq!(termios_user_bytes(&termios).len(), size_of::<Termios>());
        assert_eq!(termios2_user_bytes(&termios).len(), size_of::<Termios2>());
    }

    #[test]
    fn tcgets_branch_does_not_overwrite_termios2_tail() {
        let termios = Termios2::default();
        let bytes = termios_user_bytes(&termios);
        let mut destination = [0xa5; size_of::<Termios2>()];

        destination[..bytes.len()].copy_from_slice(&bytes);

        assert!(
            destination[size_of::<Termios>()..]
                .iter()
                .all(|&byte| byte == 0xa5)
        );
    }

    #[test]
    fn vhangup_makes_existing_slave_operations_hung_up() {
        let (_master, slave) = pty::create_pty_pair_for_test().unwrap();

        slave.hangup_io();

        let mut byte = [0_u8; 1];
        assert_eq!(slave.read_at(&mut byte, 0), Ok(0));
        assert_eq!(slave.write_at(b"x", 0), Err(AxError::Io));
        assert_eq!(
            slave.poll().bits(),
            (IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::ERROR | IoEvents::HANGUP).bits()
        );
    }

    #[test]
    fn release_to_bind_interleave_rejects_the_hangup_marker() {
        let (_master, slave) = pty::create_pty_pair_for_test().unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();

        let lifecycle = slave.terminal.lifecycle.lock();
        let concurrent_slave = slave.clone();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            // This is bind_to's admission prefix; exercising it directly
            // keeps the controlled interleave process-free.
            let _lifecycle = concurrent_slave.terminal.lifecycle.lock();
            result_tx.send(concurrent_slave.ensure_bindable()).unwrap();
        });
        started_rx.recv().unwrap();

        // This is the publish step performed before release_session/unset.
        slave.hung_up.store(true, Ordering::Release);
        drop(lifecycle);

        assert_eq!(result_rx.recv().unwrap(), Err(AxError::Io));
        waiter.join().unwrap();
    }

    #[test]
    fn termio_input_codec_discards_abi_padding() {
        let termio = Termios2::default().as_termio();
        let mut bytes = termio.to_user_bytes();
        bytes[size_of::<Termio>() - 1] = 0xa5;

        let decoded = Termio::from_user_bytes(bytes);
        let encoded = decoded.to_user_bytes();

        assert_eq!(encoded[size_of::<Termio>() - 1], 0);
        assert_eq!(
            &encoded[..size_of::<Termio>() - 1],
            &bytes[..size_of::<Termio>() - 1]
        );
    }

    /// Drains the deferred description-cleanup queue to quiescence.
    ///
    /// The queue is global and every test that drops a committed
    /// `FileDescription` publishes to it, so a single bounded batch — which is
    /// all `drain_deferred_description_resource_only_for_test` performs — only
    /// reaches quiescence when this test happens to run alone. The bound keeps
    /// a genuine leak a failure rather than a hang.
    fn drain_all_description_cleanup() {
        const MAX_DRAIN_BATCHES: usize = 1024;

        for _ in 0..MAX_DRAIN_BATCHES {
            if !has_deferred_description_cleanup_work() {
                return;
            }
            drain_deferred_description_resource_only_for_test();
        }
        assert!(
            !has_deferred_description_cleanup_work(),
            "deferred description cleanup did not reach quiescence"
        );
    }

    #[test]
    fn master_dup_releases_devpts_and_hangs_up_slave_only_after_deferred_final_close() {
        // Asserts on the shared deferred-cleanup queue, so it must not
        // interleave with other tests that publish to it.
        let _context = crate::test_support::scheduler_test_context();
        drain_all_description_cleanup();
        let (master, slave) = pty::create_pty_pair_for_test().unwrap();
        let (lease, slot) = pts::reserve_test_lease().unwrap();
        master.install_pts_lease(lease).unwrap();
        assert!(pts::test_slot_reserved(slot));

        let master_open = master.open_transport_description().unwrap().unwrap();
        let slave_open = slave.open_transport_description().unwrap().unwrap();

        // The final slave OFD close, rather than a duplicated fd close, owns
        // the master-side hangup transition.
        drop(slave_open);
        assert_eq!(
            master.endpoint.as_ref().unwrap().hangup_events().bits(),
            (IoEvents::READABLE | IoEvents::HANGUP).bits()
        );
        let slave_open = slave.open_transport_description().unwrap().unwrap();
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
