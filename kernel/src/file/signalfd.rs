use alloc::{borrow::Cow, sync::Arc};
use core::{
    mem,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use axerrno::{AxError, AxResult};
use axpoll::{IoEvents, PollSet, Pollable};
use axtask::current;
use linux_raw_sys::general::{SI_MESGQ, SI_QUEUE, SI_SIGIO, SI_TIMER};
use spin::RwLock;
use thekernel_linux_signal::{SignalInfo, SignalSet, Signo};
use zerocopy::{Immutable, IntoBytes};

use crate::{
    file::{FileLike, IoDst, IoSrc, Kstat, anon_inode_stat},
    readiness::block_on_poll_io,
    task::{AsThread, acknowledge_posix_timer_signal},
};

/// The size of signalfd_siginfo structure (128 bytes as per Linux
/// specification)
const SIGNALFD_SIGINFO_SIZE: usize = 128;

/// signalfd_siginfo structure layout
/// This matches the Linux signalfd_siginfo structure (128 bytes)
#[repr(C)]
#[derive(Immutable, IntoBytes)]
struct SignalfdSiginfo {
    ssi_signo: u32,    // Signal number
    ssi_errno: i32,    // Error number (unused)
    ssi_code: i32,     // Signal code
    ssi_pid: u32,      // PID of sender
    ssi_uid: u32,      // Real UID of sender
    ssi_fd: i32,       // File descriptor (SIGIO)
    ssi_tid: u32,      // Kernel timer ID (POSIX timers)
    ssi_band: u32,     // Band event (SIGIO)
    ssi_overrun: u32,  // POSIX timer overrun count
    ssi_trapno: u32,   // Trap number that caused signal
    ssi_status: i32,   // Exit status or signal (SIGCHLD)
    ssi_int: i32,      // Integer sent by sigqueue(2)
    ssi_ptr: u64,      // Pointer sent by sigqueue(2)
    ssi_utime: u64,    // User CPU time consumed (SIGCHLD)
    ssi_stime: u64,    // System CPU time consumed (SIGCHLD)
    ssi_addr: u64,     // Address that generated signal
    ssi_addr_lsb: u16, // Least significant bit of address
    _pad: [u8; 46],    // Padding to make it 128 bytes
}

const _: [(); SIGNALFD_SIGINFO_SIZE] = [(); mem::size_of::<SignalfdSiginfo>()];

fn sanitize_mask(mut mask: SignalSet) -> SignalSet {
    mask.remove(Signo::SIGKILL);
    mask.remove(Signo::SIGSTOP);
    mask
}

impl SignalfdSiginfo {
    /// Convert from SignalInfo to signalfd_siginfo
    fn from_signal_info(sig_info: &SignalInfo) -> Self {
        let errno = sig_info.errno();
        let mut result = SignalfdSiginfo {
            ssi_signo: sig_info.try_signo().map_or(0, |signo| signo as u32),
            ssi_errno: errno,
            ssi_code: sig_info.code(),
            ssi_pid: 0,
            ssi_uid: 0,
            ssi_fd: -1,
            ssi_tid: 0,
            ssi_band: 0,
            ssi_overrun: 0,
            ssi_trapno: 0,
            ssi_status: 0,
            ssi_int: 0,
            ssi_ptr: 0,
            ssi_utime: 0,
            ssi_stime: 0,
            ssi_addr: 0,
            ssi_addr_lsb: 0,
            _pad: [0u8; 46],
        };

        match sig_info.code() {
            SI_TIMER => {
                let timer = sig_info.timer_payload();
                result.ssi_tid = timer.tid as u32;
                result.ssi_overrun = timer.overrun.max(0) as u32;
                result.ssi_int = timer.value as i32;
                result.ssi_ptr = timer.value as u64;
            }
            SI_MESGQ | SI_QUEUE => {
                let rt = sig_info.rt_payload();
                result.ssi_pid = rt.pid as u32;
                result.ssi_uid = rt.uid;
                result.ssi_int = rt.value as i32;
                result.ssi_ptr = rt.value as u64;
            }
            SI_SIGIO => {
                let poll = sig_info.poll_payload();
                result.ssi_fd = poll.fd;
                result.ssi_band = poll.band as u32;
            }
            _ => {}
        }
        result
    }
}

pub struct Signalfd {
    mask: RwLock<SignalSet>,
    non_blocking: AtomicBool,
    poll_rx: PollSet,
}

impl Signalfd {
    pub fn new(mask: SignalSet) -> Arc<Self> {
        Arc::new(Self {
            mask: RwLock::new(sanitize_mask(mask)),
            non_blocking: AtomicBool::new(false),
            poll_rx: PollSet::new(),
        })
    }

    pub fn update_mask(&self, mask: SignalSet) {
        *self.mask.write() = sanitize_mask(mask);
        self.poll_rx.wake();
    }

    fn mask(&self) -> SignalSet {
        *self.mask.read()
    }

    /// Check if there are any pending signals matching the fd mask and the
    /// reader thread's current blocked mask.
    fn has_pending_signals(&self) -> bool {
        let mask = self.mask.read();
        let curr = current();
        let signal = &curr.as_thread().signal;
        signal.has_pending_signal_for_signalfd(&mask)
    }

    /// Dequeue a signal matching both the fd mask and the reader thread's
    /// current blocked mask. The signal manager keeps the blocked-mask
    /// snapshot and queue dequeue in one linearization domain.
    fn dequeue_signal(&self) -> Option<SignalInfo> {
        let mask = self.mask.read();
        let curr = current();
        let signal = &curr.as_thread().signal;
        signal.dequeue_signal_for_signalfd(&mask)
    }
}

impl FileLike for Signalfd {
    fn stat(&self) -> AxResult<Kstat> {
        Ok(anon_inode_stat())
    }

    fn read(&self, dst: &mut IoDst) -> AxResult<usize> {
        if dst.remaining_mut() < SIGNALFD_SIGINFO_SIZE {
            return Err(AxError::InvalidInput);
        }
        let proc_data = current().as_thread().proc_data.clone();

        block_on_poll_io(self, IoEvents::READABLE, self.nonblocking(), || {
            if let Some(sig_info) = self.dequeue_signal() {
                acknowledge_posix_timer_signal(&proc_data, &sig_info);
                // Convert SignalInfo to SignalfdSiginfo
                let sfd_info = SignalfdSiginfo::from_signal_info(&sig_info);

                // Write the structure to the destination buffer
                let bytes = sfd_info.as_bytes();
                dst.write(bytes)?;

                // Wake up other waiters if there are more signals pending
                if self.has_pending_signals() {
                    self.poll_rx.wake();
                }

                Ok(SIGNALFD_SIGINFO_SIZE)
            } else {
                Err(AxError::WouldBlock)
            }
        })
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

    fn path(&self) -> AxResult<Cow<'_, str>> {
        Ok("anon_inode:[signalfd]".into())
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
            axpoll::PollRegistration::single(&self.poll_rx, context.waker())
        } else {
            axpoll::PollRegistration::empty()
        }
    }
}

#[cfg(test)]
mod tests {
    use thekernel_linux_signal::{SignalPollPayload, SignalRtPayload, SignalTimerPayload, Signo};

    use super::*;

    #[test]
    fn timer_siginfo_projects_timer_fields() {
        let value = 0x1234_5678_abcd_ef01usize;
        let info = SignalInfo::new_timer(Signo::SIGRTMIN, SignalTimerPayload::new(17, 9, value, 0));

        let projected = SignalfdSiginfo::from_signal_info(&info);
        assert_eq!(projected.ssi_tid, 17);
        assert_eq!(projected.ssi_overrun, 9);
        assert_eq!(projected.ssi_int as u32, value as u32);
        assert_eq!(projected.ssi_ptr, value as u64);
    }

    #[test]
    fn mqueue_siginfo_projects_registration_identity_and_value() {
        let value = 0x7654_3210_abcd_ef01usize;
        let info = SignalInfo::new_rt(
            Signo::SIGRT1,
            SI_MESGQ,
            SignalRtPayload::new(42, 1000, value),
        );

        let projected = SignalfdSiginfo::from_signal_info(&info);
        assert_eq!(projected.ssi_pid, 42);
        assert_eq!(projected.ssi_uid, 1000);
        assert_eq!(projected.ssi_int as u32, value as u32);
        assert_eq!(projected.ssi_ptr, value as u64);
    }

    #[test]
    fn sigio_siginfo_projects_fd_and_band() {
        let info = SignalInfo::new_poll(Signo::SIGIO, SignalPollPayload::new(0x1234, 37));

        let projected = SignalfdSiginfo::from_signal_info(&info);
        assert_eq!(projected.ssi_fd, 37);
        assert_eq!(projected.ssi_band, 0x1234);
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
