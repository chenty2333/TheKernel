//! Linux signalfd mask policy and exact 128-byte record encoding.

use crate::{SignalInfo, SignalSet, Signo};
use linux_raw_sys::general::{SI_MESGQ, SI_QUEUE, SI_SIGIO, SI_TIMER};

/// A signalfd mask after removing signals Linux never exposes through signalfd.
#[derive(Clone, Copy, Debug)]
pub struct SignalfdMask(SignalSet);
impl SignalfdMask {
    /// Builds the effective mask; `SIGKILL` and `SIGSTOP` are ignored by Linux.
    pub fn new(mut mask: SignalSet) -> Self {
        mask.remove(Signo::SIGKILL);
        mask.remove(Signo::SIGSTOP);
        Self(mask)
    }
    /// Returns the mask used for readiness and dequeue selection.
    pub const fn signals(self) -> SignalSet {
        self.0
    }
    /// Returns whether `signo` is eligible for this descriptor.
    pub fn contains(self, signo: Signo) -> bool {
        self.0.has(signo)
    }
}

/// Linux x86_64 `struct signalfd_siginfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignalfdSiginfo {
    pub signo: u32,
    pub errno: i32,
    pub code: i32,
    pub pid: u32,
    pub uid: u32,
    pub fd: i32,
    pub tid: u32,
    pub band: u32,
    pub overrun: u32,
    pub trapno: u32,
    pub status: i32,
    pub int: i32,
    pub ptr: u64,
    pub utime: u64,
    pub stime: u64,
    pub addr: u64,
    pub addr_lsb: u16,
    pub pad2: u16,
    pub syscall: i32,
    pub call_addr: u64,
    pub arch: u32,
    pub pad: [u8; 28],
}
const _: () = assert!(core::mem::size_of::<SignalfdSiginfo>() == 128);
const _: () = assert!(core::mem::align_of::<SignalfdSiginfo>() == 8);

impl SignalfdSiginfo {
    /// Produces one self-contained ABI record from a dequeued signal snapshot.
    pub fn encode(info: &SignalInfo) -> Self {
        let mut out = Self {
            signo: info.signo() as u32,
            errno: info.errno(),
            code: info.code(),
            pid: info.pid(),
            uid: info.uid(),
            fd: 0,
            tid: 0,
            band: 0,
            overrun: 0,
            trapno: 0,
            status: 0,
            int: 0,
            ptr: 0,
            utime: 0,
            stime: 0,
            addr: 0,
            addr_lsb: 0,
            pad2: 0,
            syscall: 0,
            call_addr: 0,
            arch: 0,
            pad: [0; 28],
        };
        if info.code() == SI_TIMER {
            let p = info.timer_payload();
            out.tid = p.tid as u32;
            out.overrun = p.overrun as u32;
            out.int = p.value as i32;
            out.ptr = p.value as u64;
        } else if matches!(info.code(), SI_MESGQ | SI_QUEUE) {
            let p = info.rt_payload();
            out.pid = p.pid as u32;
            out.uid = p.uid;
            out.int = p.value as i32;
            out.ptr = p.value as u64;
        } else if info.code() == SI_SIGIO {
            let p = info.poll_payload();
            out.band = p.band as u32;
            out.fd = p.fd;
        } else if info.signo() == Signo::SIGSYS {
            out.call_addr = info.sigsys_call_address() as u64;
            out.syscall = info.sigsys_syscall();
            out.arch = info.sigsys_arch();
        } else if matches!(
            info.signo(),
            Signo::SIGILL | Signo::SIGFPE | Signo::SIGSEGV | Signo::SIGBUS | Signo::SIGTRAP
        ) {
            out.addr = info.fault_address() as u64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SignalPollPayload, SignalTimerPayload};
    #[test]
    fn mask_ignores_uncatchable_signals() {
        let mut mask = SignalSet::default();
        mask.add(Signo::SIGKILL);
        mask.add(Signo::SIGINT);
        let mask = SignalfdMask::new(mask);
        assert!(!mask.contains(Signo::SIGKILL));
        assert!(mask.contains(Signo::SIGINT));
    }
    #[test]
    fn records_are_exactly_128_bytes_and_encode_payloads() {
        let timer = SignalfdSiginfo::encode(&SignalInfo::new_timer(
            Signo::SIGRT1,
            SignalTimerPayload::new(4, 5, 6, 0),
        ));
        assert_eq!(timer.tid, 4);
        assert_eq!(timer.overrun, 5);
        assert_eq!(timer.ptr, 6);
        let poll = SignalfdSiginfo::encode(&SignalInfo::new_poll(
            Signo::SIGIO,
            SignalPollPayload::new(7, 8),
        ));
        assert_eq!(poll.band, 7);
        assert_eq!(poll.fd, 8);
    }
}
