use alloc::{borrow::Cow, sync::Arc};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use spin::RwLock;
use tk_linux_signal::{SignalInfo, SignalSet, SignalfdMask, SignalfdSiginfo};

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, anon_inode_stat},
    readiness::block_on_poll_io,
    task::{AsThread, ProcessData, acknowledge_posix_timer_signal},
};

/// The size of signalfd_siginfo structure (128 bytes as per Linux
/// specification)
const SIGNALFD_SIGINFO_SIZE: usize = 128;

const _: [(); SIGNALFD_SIGINFO_SIZE] = [(); mem::size_of::<SignalfdSiginfo>()];

pub struct Signalfd {
    mask: RwLock<SignalSet>,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
}

impl Signalfd {
    pub fn new(mask: SignalSet) -> Arc<Self> {
        Arc::new(Self {
            mask: RwLock::new(SignalfdMask::new(mask).signals()),
            non_blocking: AtomicBool::new(false),
            poll_rx: PollSet::new(),
        })
    }

    pub fn update_mask(&self, mask: SignalSet) {
        *self.mask.write() = SignalfdMask::new(mask).signals();
        self.poll_rx.wake();
    }

    fn mask(&self) -> SignalSet {
        *self.mask.read()
    }

    /// Check if any pending signal matches the fd mask alone.
    ///
    /// Like Linux's `signalfd_poll` (fs/signalfd.c:52-68), which runs
    /// `next_signal` over both pending queues with `ctx->sigmask` only
    /// (lines 59-61), the reader's blocked mask is not consulted.
    fn has_pending_signals(&self) -> bool {
        let mask = self.mask.read();
        let curr = current();
        let signal = &curr.as_thread().signal;
        signal.has_pending_signal_for_signalfd(&mask)
    }

    /// Dequeue a signal matching the fd mask alone.
    ///
    /// Like Linux's `signalfd_dequeue()` (fs/signalfd.c:162 and 177), the fd
    /// mask is passed to `dequeue_signal` (kernel/signal.c:618-637) as-is and
    /// is NOT intersected with the reader thread's blocked mask: a signalfd
    /// read may dequeue and consume a pending signal that is not blocked.
    fn dequeue_signal(&self) -> Option<SignalInfo> {
        let mask = self.mask.read();
        let curr = current();
        let signal = &curr.as_thread().signal;
        signal.dequeue_signal(&mask)
    }

    /// Dequeues one signal selected by the fd mask and writes one
    /// `signalfd_siginfo` record into `dst`, returning the record size.
    /// Returns [`AxError::WouldBlock`] when no matching signal is pending.
    fn dequeue_record(&self, proc_data: &ProcessData, dst: &mut IoDst) -> AxResult<usize> {
        let Some(sig_info) = self.dequeue_signal() else {
            return Err(AxError::WouldBlock);
        };
        acknowledge_posix_timer_signal(proc_data, &sig_info);
        let sfd_info = SignalfdSiginfo::encode(&sig_info);
        let bytes = &bytemuck::bytes_of(&sfd_info)[..SIGNALFD_SIGINFO_SIZE];
        dst.write(bytes)?;
        if self.has_pending_signals() {
            self.poll_rx.wake();
        }
        Ok(SIGNALFD_SIGINFO_SIZE)
    }

    /// Reads signalfd records without changing the OFD's O_NONBLOCK bit.
    ///
    /// Mirrors Linux's `signalfd_read` (fs/signalfd.c:205-220): the buffer
    /// must hold at least one `signalfd_siginfo` record or EINVAL is
    /// returned; at most `remaining / record size` records are produced per
    /// call, only the first record honors the caller's blocking mode, every
    /// later record is dequeued with forced non-blocking (`nonblock = 1`,
    /// fs/signalfd.c:216) and an empty queue stops the loop; once at least
    /// one record was produced, later errors are swallowed in favor of the
    /// accumulated byte count (`return total ? total : ret`).
    pub(crate) fn read_with_nonblocking(
        &self,
        dst: &mut IoDst,
        nonblocking: bool,
    ) -> AxResult<usize> {
        if dst.remaining_mut() < SIGNALFD_SIGINFO_SIZE {
            return Err(AxError::InvalidInput);
        }
        let max_records = dst.remaining_mut() / SIGNALFD_SIGINFO_SIZE;
        let proc_data = current().as_thread().proc_data.clone();

        // The first record follows the caller's blocking mode.
        let mut total = block_on_poll_io(self, IoEvents::READABLE, nonblocking, || {
            self.dequeue_record(&proc_data, dst)
        })?;

        // From the second record on, dequeue without ever waiting.
        for _ in 1..max_records {
            match self.dequeue_record(&proc_data, dst) {
                Ok(bytes) => total += bytes,
                Err(_) => break,
            }
        }
        Ok(total)
    }
}

impl FileLike for Signalfd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        self.read_with_nonblocking(dst, self.nonblocking())
    }

    fn write(&self, _src: &mut IoSrc) -> AxResult<usize> {
        // signalfd is read-only
        Err(AxError::BadFileDescriptor)
    }

    fn nonblocking(&self) -> bool {
        self.non_blocking.load(Ordering::Acquire)
    }

    fn set_nonblocking(&self, non_blocking: bool) -> AxResult {
        self.non_blocking.store(non_blocking, Ordering::Release);
        Ok(())
    }

    fn path(&self) -> AxResult<Cow<'_, axfs_ng_vfs::FsPath>> {
        Ok(Cow::Borrowed(axfs_ng_vfs::FsPath::new(
            b"anon_inode:[signalfd]",
        )))
    }
}

impl Pollable for Signalfd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::READABLE, self.has_pending_signals());
        events
    }

    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        if events.contains(IoEvents::READABLE) {
            // The OFD may be shared across fork, but queues belong to the
            // task performing this registration/read, not the fd creator.
            // Own the sources so persistent epoll interests do not retain a
            // Thread or ProcessData and cannot outlive a borrowed task guard.
            // Sibling sends/mask changes notify this same process source even
            // when another thread originally installed the epoll interest.
            let curr = current();
            let thread = curr.as_thread();
            let mut registration = axpoll::PreparedPollRegistration::try_new(2)?;
            registration.arm(&self.poll_rx, context.waker())?;
            registration.arm_owned(
                thread.proc_data.signal_pending_event.clone(),
                context.waker(),
            )?;
            registration.commit()
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::SI_MESGQ;
    use tk_linux_signal::{SignalPollPayload, SignalRtPayload, SignalTimerPayload, Signo};

    use super::*;

    #[test]
    fn timer_siginfo_projects_timer_fields() {
        let value = 0x1234_5678_abcd_ef01usize;
        let info = SignalInfo::new_timer(Signo::SIGRTMIN, SignalTimerPayload::new(17, 9, value, 0));

        let projected = SignalfdSiginfo::encode(&info);
        assert_eq!(projected.tid, 17);
        assert_eq!(projected.overrun, 9);
        assert_eq!(projected.ptr, value as u64);
    }

    #[test]
    fn mqueue_siginfo_projects_registration_identity_and_value() {
        let value = 0x7654_3210_abcd_ef01usize;
        let info = SignalInfo::new_rt(
            Signo::SIGRT1,
            SI_MESGQ,
            SignalRtPayload::new(42, 1000, value),
        );

        let projected = SignalfdSiginfo::encode(&info);
        assert_eq!(projected.pid, 42);
        assert_eq!(projected.uid, 1000);
        assert_eq!(projected.int as u32, value as u32);
        assert_eq!(projected.ptr, value as u64);
    }

    #[test]
    fn sigio_siginfo_projects_fd_and_band() {
        let info = SignalInfo::new_poll(Signo::SIGIO, SignalPollPayload::new(0x1234, 37));

        let projected = SignalfdSiginfo::encode(&info);
        assert_eq!(projected.fd, 37);
        assert_eq!(projected.band, 0x1234);
    }

    #[test]
    fn mask_excludes_uncatchable_signals_on_create_and_update() {
        let mut requested = SignalSet::default();
        requested.add(Signo::SIGKILL);
        requested.add(Signo::SIGSTOP);
        requested.add(Signo::SIGUSR1);

        let signalfd = Signalfd::new(requested);
        let mask = signalfd.mask();
        assert!(!mask.has(Signo::SIGKILL));
        assert!(!mask.has(Signo::SIGSTOP));
        assert!(mask.has(Signo::SIGUSR1));

        signalfd.update_mask(requested);
        let mask = signalfd.mask();
        assert!(!mask.has(Signo::SIGKILL));
        assert!(!mask.has(Signo::SIGSTOP));
        assert!(mask.has(Signo::SIGUSR1));
    }
}
