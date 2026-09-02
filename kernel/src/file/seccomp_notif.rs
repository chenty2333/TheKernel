//! Seccomp user-notification listener open-file descriptions.
//!
//! The listener owns only the broker queue.  Filter evaluation retains an
//! opaque listener id, so closing the last listener FD can cancel outstanding
//! requests without changing immutable seccomp filter nodes.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult, LinuxError};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, PollSet, Pollable};
use bytemuck::{Pod, Zeroable};
use spin::Mutex;
use thekernel_linux_seccomp::{
    SECCOMP_ADDFD_FLAG_SEND, SECCOMP_ADDFD_FLAG_SETFD, SECCOMP_IOCTL_NOTIF_ADDFD,
    SECCOMP_IOCTL_NOTIF_ID_VALID, SECCOMP_IOCTL_NOTIF_RECV, SECCOMP_IOCTL_NOTIF_SEND,
    SECCOMP_IOCTL_NOTIF_SET_FLAGS, SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP,
    SECCOMP_USER_NOTIF_FLAG_CONTINUE, SeccompData,
};

use crate::{
    file::{FdTable, FileLike, IoctlContext, Kstat, anon_inode_stat},
    readiness::{block_on_poll_io, block_on_poll_io_interruptible_if},
    task::{AsThread, PidNamespace, has_pending_sigkill},
};

const MAX_NOTIFICATIONS: usize = 1024;

#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct RawData {
    nr: i32,
    arch: u32,
    ip: u64,
    args: [u64; 6],
}
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct RawNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: RawData,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct RawResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct RawAddfd {
    id: u64,
    flags: u32,
    srcfd: u32,
    newfd: u32,
    newfd_flags: u32,
}

const _: [(); 64] = [(); mem::size_of::<RawData>()];
const _: [(); 80] = [(); mem::size_of::<RawNotif>()];
const _: [(); 24] = [(); mem::size_of::<RawResp>()];
const _: [(); 24] = [(); mem::size_of::<RawAddfd>()];

#[derive(Clone, Copy)]
enum Reply {
    Value(i64),
    Errno(i32),
    Continue,
}
struct Request {
    notification: RawNotif,
    /// Kernel-wide TID and namespace captured at the filter hit.  The
    /// receiver's PID namespace, rather than the listener creator's, renders
    /// this identity during RECV; retaining the pair also prevents a numeric
    /// PID reuse from changing a queued notification's subject.
    target_global_pid: thekernel_linux_process_adapter::Pid,
    target_pid_ns: Arc<PidNamespace>,
    target_task_id: u64,
    target_files: Arc<FdTable>,
    target_nofile: usize,
    received: bool,
    /// RECV has claimed this request but has not yet made the 80-byte record
    /// visible to userspace. SEND/ADDFD must not race that usercopy boundary.
    copyout_pending: bool,
    reply: Option<Reply>,
}
struct ListenerState {
    requests: Vec<Request>,
    closed: bool,
    cancellation_sequence: u64,
    /// A bounded per-request tombstone for the latest cancellation wake. A
    /// receiver snapshots its sequence before arming; it never mistakes an
    /// arbitrary state change for cancellation of an in-flight queue item.
    last_cancellation: Option<CancelledRequest>,
}

#[derive(Clone, Copy)]
struct CancelledRequest {
    id: u64,
    sequence: u64,
}

impl ListenerState {
    fn record_cancellation(&mut self, id: u64) {
        self.cancellation_sequence = self.cancellation_sequence.wrapping_add(1);
        self.last_cancellation = Some(CancelledRequest {
            id,
            sequence: self.cancellation_sequence,
        });
    }
}

/// A listener is an anon-inode and remains shared across dup/fork just like a
/// Linux seccomp filter listener FD.
pub(crate) struct SeccompListener {
    id: u64,
    next_request: AtomicU64,
    state: Mutex<ListenerState>,
    nonblocking: AtomicBool,
    wait_killable_recv: bool,
    flags: AtomicU64,
    waiters: PollSet,
}

static NEXT_LISTENER: AtomicU64 = AtomicU64::new(1);
static LISTENERS: Mutex<Vec<(u64, Weak<SeccompListener>)>> = Mutex::new(Vec::new());

impl SeccompListener {
    pub(crate) fn try_new(wait_killable_recv: bool) -> AxResult<Arc<Self>> {
        let id = NEXT_LISTENER.fetch_add(1, Ordering::Relaxed).max(1);
        let listener = Arc::try_new(Self {
            id,
            next_request: AtomicU64::new(1),
            state: Mutex::new(ListenerState {
                requests: Vec::new(),
                closed: false,
                cancellation_sequence: 0,
                last_cancellation: None,
            }),
            nonblocking: AtomicBool::new(false),
            wait_killable_recv,
            flags: AtomicU64::new(0),
            waiters: PollSet::new(),
        })
        .map_err(|_| AxError::NoMemory)?;
        let mut listeners = LISTENERS.lock();
        listeners.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        listeners.push((id, Arc::downgrade(&listener)));
        Ok(listener)
    }
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// Queue one filter hit and wait for the broker's terminal response.
    pub(crate) fn notify(
        &self,
        target_global_pid: thekernel_linux_process_adapter::Pid,
        target_pid_ns: Arc<PidNamespace>,
        target_task_id: u64,
        data: SeccompData,
        target_files: Arc<FdTable>,
        target_nofile: usize,
    ) -> AxResult<NotificationResult> {
        let id = self.next_request.fetch_add(1, Ordering::Relaxed).max(1);
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(LinuxError::ENOSYS.into());
            }
            if state.requests.len() == MAX_NOTIFICATIONS {
                return Err(AxError::WouldBlock);
            }
            state
                .requests
                .try_reserve(1)
                .map_err(|_| AxError::NoMemory)?;
            state.requests.push(Request {
                notification: RawNotif {
                    id,
                    // PID is receiver-relative and is materialized only by
                    // RECV under that ioctl caller's namespace snapshot.
                    pid: 0,
                    flags: 0,
                    data: RawData {
                        nr: data.number,
                        arch: data.architecture,
                        ip: data.instruction_pointer,
                        args: data.arguments,
                    },
                },
                target_global_pid,
                target_pid_ns,
                target_task_id,
                target_files,
                target_nofile,
                received: false,
                copyout_pending: false,
                reply: None,
            });
        }
        self.wake_waiters();
        let result = block_on_poll_io(self, IoEvents::READABLE, false, || self.take_reply(id));
        // A signal which interrupts the tracee's syscall invalidates its
        // notification before returning to the signal path.  Leaving it in
        // the listener after EINTR would let a broker act on a later syscall
        // which happened to reuse the same task/files context.  Completion
        // wins over interruption in `block_on_poll_io`, so this removal only
        // applies while there is no terminal response to consume.
        if matches!(&result, Err(AxError::Interrupted)) {
            self.cancel_request(id);
        }
        result
    }

    fn take_reply(&self, id: u64) -> AxResult<NotificationResult> {
        let mut state = self.state.lock();
        let Some(index) = state
            .requests
            .iter()
            .position(|request| request.notification.id == id)
        else {
            return Err(LinuxError::ENOSYS.into());
        };
        match state.requests[index].reply {
            None => Err(AxError::WouldBlock),
            Some(Reply::Value(value)) => {
                state.requests.swap_remove(index);
                Ok(NotificationResult::Value(value))
            }
            Some(Reply::Errno(errno)) => {
                state.requests.swap_remove(index);
                Ok(NotificationResult::Errno(errno))
            }
            Some(Reply::Continue) => {
                state.requests.swap_remove(index);
                Ok(NotificationResult::Continue)
            }
        }
    }

    fn recv(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        // Linux requires the complete 80-byte input buffer to be zero before
        // it waits or consumes a queue entry.  This catches stale IDs/flags
        // and makes an EFAULT leave the request available to another RECV.
        let input: RawNotif = read_pod(context, arg)?;
        if bytemuck::bytes_of(&input).iter().any(|byte| *byte != 0) {
            return Err(AxError::InvalidInput);
        }
        // Capture before arming readiness. A task teardown which removes a
        // request after this point is terminal for this particular RECV; a
        // fresh RECV starts at the new epoch and may wait for later work.
        let expected_cancellation_sequence = self.state.lock().cancellation_sequence;
        let mut armed_for_cancellation = false;
        let claim = || {
            let mut state = self.state.lock();
            if state.closed {
                return Err(LinuxError::ENOSYS.into());
            }
            if let Some(request) = state.requests.iter_mut().find(|request| !request.received) {
                // A new INIT request wins over any unrelated cancellation
                // wake: RECV consumes the available request instead of
                // converting a separate tracee's teardown into ENOENT.
                request.received = true;
                request.copyout_pending = true;
                let mut notification = request.notification;
                // Listener FDs are transferable (fork/dup/SCM), so the current
                // ioctl receiver is authoritative. Strict rendering deliberately
                // gives zero for both an invisible namespace and a no-longer-bound
                // target PID; it never falls back to the raw kernel number.
                notification.pid = context
                    .caller_process()
                    .pid_ns()
                    .visible_pid_for(&request.target_pid_ns, request.target_global_pid)
                    .unwrap_or(0) as u32;
                return Ok(notification);
            }
            if armed_for_cancellation
                && state.last_cancellation.is_some_and(|tombstone| {
                    tombstone.sequence != expected_cancellation_sequence && tombstone.id != 0
                })
            {
                return Err(LinuxError::ENOENT.into());
            }
            armed_for_cancellation = true;
            Err(AxError::WouldBlock)
        };
        let notification = if self.wait_killable_recv {
            block_on_poll_io_interruptible_if(
                self,
                IoEvents::READABLE,
                self.nonblocking(),
                claim,
                || has_pending_sigkill(axtask::current().as_thread()),
            )?
        } else {
            block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), claim)?
        };
        if let Err(error) = context
            .user_memory()
            .write_bytes(arg, bytemuck::bytes_of(&notification))
        {
            let mut state = self.state.lock();
            if let Some(request) = state
                .requests
                .iter_mut()
                .find(|request| request.notification.id == notification.id)
                && request.reply.is_none()
            {
                // SEND cannot install a reply while copyout_pending, so a
                // present request always returns to INIT on EFAULT.
                request.received = false;
                request.copyout_pending = false;
            }
            drop(state);
            self.wake_waiters();
            return Err(crate::mm::map_usercopy_error(error));
        }
        let mut state = self.state.lock();
        if let Some(request) = state
            .requests
            .iter_mut()
            .find(|request| request.notification.id == notification.id)
        {
            request.copyout_pending = false;
        }
        drop(state);
        // The successful copy is the SENT publication. Only now may poll
        // advertise writable and a supervisor issue SEND/ADDFD.
        self.wake_waiters();
        Ok(0)
    }

    fn send(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let response: RawResp = read_pod(context, arg)?;
        if response.flags & !SECCOMP_USER_NOTIF_FLAG_CONTINUE != 0 {
            return Err(AxError::InvalidInput);
        }
        let mut state = self.state.lock();
        let Some(request) = state
            .requests
            .iter_mut()
            .find(|request| request.notification.id == response.id)
        else {
            return Err(LinuxError::ENOENT.into());
        };
        // The ID exists but is not in the supervisor-owned SENT state. Linux
        // reports this transition race as EINPROGRESS; only an absent/cancelled
        // ID is ENOENT.
        if !request.received || request.copyout_pending || request.reply.is_some() {
            return Err(LinuxError::EINPROGRESS.into());
        }
        request.reply = Some(if response.flags == SECCOMP_USER_NOTIF_FLAG_CONTINUE {
            if response.error != 0 || response.val != 0 {
                return Err(AxError::InvalidInput);
            }
            Reply::Continue
        } else if response.error != 0 {
            // UAPI carries the already-negative Linux errno.  Positive values
            // are malformed rather than an alternate spelling for errno.
            if response.error > 0 {
                return Err(AxError::InvalidInput);
            }
            Reply::Errno(response.error)
        } else {
            Reply::Value(response.val)
        });
        drop(state);
        self.wake_waiters();
        Ok(0)
    }

    fn id_valid(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let id: u64 = read_pod(context, arg)?;
        let state = self.state.lock();
        // Linux exposes an ID only after RECV handed it to a supervisor. An
        // INIT request, a replied request, and a cancelled one are all ENOENT.
        if state.requests.iter().any(|request| {
            request.notification.id == id
                && request.received
                && !request.copyout_pending
                && request.reply.is_none()
        }) {
            Ok(0)
        } else {
            Err(LinuxError::ENOENT.into())
        }
    }

    fn addfd(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let request: RawAddfd = read_pod(context, arg)?;
        if request.flags & !(SECCOMP_ADDFD_FLAG_SETFD | SECCOMP_ADDFD_FLAG_SEND) != 0 {
            return Err(AxError::InvalidInput);
        }
        if request.flags & SECCOMP_ADDFD_FLAG_SETFD == 0 && request.newfd != 0 {
            return Err(AxError::InvalidInput);
        }
        if request.newfd_flags & !linux_raw_sys::general::O_CLOEXEC != 0 {
            return Err(AxError::InvalidInput);
        }
        // Linux resolves the source descriptor before looking up the
        // notification. Therefore EBADF wins over an unknown request ID.
        let source = context.get_file_like(request.srcfd as i32)?;
        let mut state = self.state.lock();
        let Some(entry) = state
            .requests
            .iter_mut()
            .find(|entry| entry.notification.id == request.id)
        else {
            return Err(LinuxError::ENOENT.into());
        };
        if !entry.received || entry.copyout_pending || entry.reply.is_some() {
            return Err(LinuxError::EINPROGRESS.into());
        }
        // Holding the notification gate after source resolution makes SEND,
        // close, and target exit unable to turn this injection into an action
        // on a different request between validation and publication.
        // The target is the stopped task's files table, not the supervisor's.
        // Retaining the exact OFD also preserves status flags, locks, and
        // close lifetime exactly as dup-style descriptor injection requires.
        let target_fd = if request.flags & SECCOMP_ADDFD_FLAG_SETFD != 0 {
            if request.newfd as usize >= entry.target_nofile {
                return Err(AxError::BadFileDescriptor);
            }
            entry.target_files.replace_description_at(
                source.description.clone(),
                request.newfd as i32,
                request.newfd_flags & linux_raw_sys::general::O_CLOEXEC != 0,
            )?;
            request.newfd as i32
        } else {
            entry.target_files.add_at_least(
                source.description.clone(),
                0,
                entry.target_nofile,
                request.newfd_flags & linux_raw_sys::general::O_CLOEXEC != 0,
            )?
        };
        if request.flags & SECCOMP_ADDFD_FLAG_SEND != 0 {
            entry.reply = Some(Reply::Value(target_fd as i64));
        }
        drop(state);
        self.wake_waiters();
        Ok(target_fd as usize)
    }

    fn set_flags(&self, context: &IoctlContext, arg: usize) -> AxResult<usize> {
        let flags: u64 = read_pod(context, arg)?;
        if flags & !SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP != 0 {
            return Err(AxError::InvalidInput);
        }
        self.flags.store(flags, Ordering::Release);
        self.wake_waiters();
        Ok(0)
    }

    fn wake_waiters(&self) {
        // PollSet wake is synchronous. Keeping the requested bit makes the
        // SET_FLAGS state observable and preserves the Linux ordering: queue
        // publication happens-before an explicitly synchronous wake.
        let _sync = self.flags.load(Ordering::Acquire) & SECCOMP_USER_NOTIF_FD_SYNC_WAKE_UP != 0;
        self.waiters.wake();
    }

    fn cancel_request(&self, id: u64) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .requests
            .iter()
            .position(|request| request.notification.id == id)
        {
            state.requests.swap_remove(index);
            state.record_cancellation(id);
        }
        drop(state);
        self.wake_waiters();
    }

    fn close(&self) {
        let mut state = self.state.lock();
        state.closed = true;
        // There can be no successful reply after the final listener OFD has
        // dropped. Drop all requests now rather than retaining abandoned
        // tracee/file-table references through a lingering mapping.
        state.requests.clear();
        drop(state);
        self.wake_waiters();
    }
}

/// Resolves a listener held by a published immutable filter metadata record.
/// A dead or last-closed listener deliberately looks absent to the filter and
/// therefore gives the intercepted syscall Linux's ENOSYS result.
pub(crate) fn listener_by_id(id: u64) -> Option<Arc<SeccompListener>> {
    if id == 0 {
        return None;
    }
    let mut listeners = LISTENERS.lock();
    let index = listeners
        .iter()
        .position(|(candidate, _)| *candidate == id)?;
    match listeners[index].1.upgrade() {
        Some(listener) => Some(listener),
        None => {
            listeners.swap_remove(index);
            None
        }
    }
}

/// Cancels requests owned by a task that is leaving its current image.  This
/// is deliberately independent of listener-FD lifetime: seccomp filters may
/// survive exec and a supervisor can outlive its tracee.
pub(crate) fn cancel_requests_for_task(task_id: u64) {
    let listeners = LISTENERS.lock();
    for (_, listener) in listeners.iter() {
        if let Some(listener) = listener.upgrade() {
            let mut state = listener.state.lock();
            // Do not retain the stopped task's file table after exec/exit.
            // The intercepted tracee sees its removed request as ENOSYS;
            // broker ID operations observe ENOENT, while a RECV already
            // asleep across this cancellation observes the epoch's ENOENT.
            let mut cancelled = None;
            state.requests.retain(|request| {
                let remove = request.target_task_id == task_id;
                if remove {
                    cancelled = Some(request.notification.id);
                }
                !remove
            });
            if let Some(id) = cancelled {
                state.record_cancellation(id);
            }
            drop(state);
            listener.wake_waiters();
        }
    }
}

fn read_pod<T: Pod>(context: &IoctlContext, address: usize) -> AxResult<T> {
    context
        .user_memory()
        .read_value(address as *const T)
        .map_err(crate::mm::map_usercopy_error)
}

pub(crate) enum NotificationResult {
    Value(i64),
    Errno(i32),
    Continue,
}

impl FileLike for SeccompListener {
    // `pre_close` runs at the last descriptor only after descriptor lifetime
    // has excluded queued SCM_RIGHTS transfer custody. Epoll/VMA retention is
    // not transfer authority and therefore cannot keep an orphaned listener
    // alive; `final_close` remains the hard final-OFD fallback.
    fn pre_close(&self) {
        self.close();
    }
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }
    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:seccomp notify",
        )))
    }
    fn ioctl(&self, context: &IoctlContext, cmd: u32, arg: usize) -> AxResult<usize> {
        match cmd {
            SECCOMP_IOCTL_NOTIF_RECV => self.recv(context, arg),
            SECCOMP_IOCTL_NOTIF_SEND => self.send(context, arg),
            SECCOMP_IOCTL_NOTIF_ID_VALID => self.id_valid(context, arg),
            SECCOMP_IOCTL_NOTIF_ADDFD => self.addfd(context, arg),
            SECCOMP_IOCTL_NOTIF_SET_FLAGS => self.set_flags(context, arg),
            _ => Err(AxError::InvalidInput),
        }
    }
    fn nonblocking(&self) -> bool {
        self.nonblocking.load(Ordering::Acquire)
    }
    fn set_nonblocking(&self, value: bool) -> AxResult {
        self.nonblocking.store(value, Ordering::Release);
        self.wake_waiters();
        Ok(())
    }
    fn final_close(&self) {
        self.close();
    }
}
impl Pollable for SeccompListener {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        if state.closed {
            return IoEvents::HANGUP;
        }
        let mut events = IoEvents::empty();
        events.set(
            IoEvents::READABLE,
            state.requests.iter().any(|request| !request.received),
        );
        // A received, unreplied request accepts SEND/ADDFD and is therefore
        // writable in both the generic and WRNORM readiness classes.
        let can_reply = state
            .requests
            .iter()
            .any(|request| request.received && !request.copyout_pending && request.reply.is_none());
        events.set(IoEvents::WRITABLE, can_reply);
        events.set(IoEvents::WRITE_NORMAL, can_reply);
        events
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(
            IoEvents::READABLE | IoEvents::WRITABLE | IoEvents::WRITE_NORMAL | IoEvents::HANGUP,
        ) {
            PollRegistration::single(&self.waiters, context.waker())
        } else {
            PollRegistration::empty()
        }
    }
}
