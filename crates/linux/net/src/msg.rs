//! Pure Linux message-transfer rules for `sendto`/`sendmsg`/`sendmmsg` and
//! `recvfrom`/`recvmsg`/`recvmmsg`.
//!
//! Every rule here is transcribed from Linux v7.2.3:
//!
//! * `include/linux/socket.h` — the `MSG_*` bit values and
//!   `MSG_INTERNAL_SENDMSG_FLAGS`.
//! * `net/socket.c:__sys_sendmsg`, `net/socket.c:__sys_sendmmsg`,
//!   `net/socket.c:__sys_recvmsg` — the `MSG_CMSG_COMPAT` rejection that runs
//!   before the descriptor is resolved.
//! * `net/ipv4/tcp.c:tcp_recvmsg_locked` driven by
//!   `include/net/sock.h:sock_rcvlowat` — the `MSG_WAITALL` completion target.
//! * `fs/select.c:poll_select_set_timeout`, `kernel/time/time.c`
//!   `set_normalized_timespec64`/`timespec64_sub` and
//!   `net/socket.c:do_recvmmsg` — the per-batch relative deadline.
//!
//! Nothing in this module owns a socket, a descriptor, or a namespace.

/// Linux `MSG_OOB`.
pub const MSG_OOB: u32 = 0x1;
/// Linux `MSG_DONTROUTE` (also `MSG_TRYHARD`).
pub const MSG_DONTROUTE: u32 = 0x4;
/// Linux `MSG_DONTWAIT`.
pub const MSG_DONTWAIT: u32 = 0x40;
/// Linux `MSG_CONFIRM`.
pub const MSG_CONFIRM: u32 = 0x800;
/// Linux `MSG_NOSIGNAL`.
pub const MSG_NOSIGNAL: u32 = 0x4000;
/// Linux `MSG_MORE`.
pub const MSG_MORE: u32 = 0x8000;
/// Linux `MSG_WAITFORONE` (aliases the internal `MSG_SENDPAGE_NOPOLICY`).
pub const MSG_WAITFORONE: u32 = 0x1_0000;
/// Linux `MSG_SPLICE_PAGES`.
pub const MSG_SPLICE_PAGES: u32 = 0x0800_0000;
/// Linux `MSG_CMSG_CLOEXEC`.
pub const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;
/// Linux `MSG_CMSG_COMPAT`, nonzero because x86_64 enables `CONFIG_COMPAT`.
pub const MSG_CMSG_COMPAT: u32 = 0x8000_0000;

const MSG_SENDPAGE_NOPOLICY: u32 = 0x1_0000;
const MSG_NO_SHARED_FRAGS: u32 = 0x8_0000;
const MSG_SENDPAGE_DECRYPTED: u32 = 0x10_0000;

/// `MSG_INTERNAL_SENDMSG_FLAGS`: kernel-internal bits that
/// `____sys_sendmsg()` and `__sys_sendto()` clear before the protocol sees the
/// message, so userspace cannot smuggle them into a transport.
pub const MSG_INTERNAL_SENDMSG_FLAGS: u32 =
    MSG_SPLICE_PAGES | MSG_SENDPAGE_NOPOLICY | MSG_SENDPAGE_DECRYPTED | MSG_NO_SHARED_FRAGS;

/// `net/socket.c:__sys_sendmsg`, `__sys_sendmmsg`, `__sys_recvmsg` and
/// `SYSCALL_DEFINE5(recvmmsg)` all run `forbid_cmsg_compat` and return `EINVAL`
/// for `MSG_CMSG_COMPAT` *before* they resolve `fd`.  `sendto`/`recvfrom` never
/// perform this check.
pub const fn compat_flag_errno(flags: u32) -> Option<i32> {
    if flags & MSG_CMSG_COMPAT != 0 {
        Some(22)
    } else {
        None
    }
}

/// Removes the bits Linux clears on entry to `sendto` and `sendmsg`.
pub const fn strip_internal_sendmsg_flags(flags: u32) -> u32 {
    flags & !MSG_INTERNAL_SENDMSG_FLAGS
}

/// Nanoseconds per second, the normalisation base of `struct timespec64`.
pub const NSEC_PER_SEC: i64 = 1_000_000_000;

/// A normalised `struct timespec64`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timespec64 {
    /// Whole seconds; never negative in a valid value.
    pub sec: i64,
    /// Nanoseconds in `0..NSEC_PER_SEC`.
    pub nsec: i64,
}

impl Timespec64 {
    /// The zero interval, which `do_recvmmsg()` also uses as "expired".
    pub const ZERO: Self = Self { sec: 0, nsec: 0 };

    /// Builds a raw pair; callers that accept user input must validate it.
    pub const fn new(sec: i64, nsec: i64) -> Self {
        Self { sec, nsec }
    }

    /// `timespec64_valid()` from `include/linux/time64.h`.
    pub const fn is_valid(self) -> bool {
        self.sec >= 0 && self.nsec >= 0 && self.nsec < NSEC_PER_SEC
    }

    /// `do_recvmmsg()` treats a zero remaining interval as an expired batch.
    pub const fn is_zero(self) -> bool {
        self.sec == 0 && self.nsec == 0
    }
}

/// `fs/select.c:poll_select_set_timeout()`: converts the user's relative
/// timeout into the absolute monotonic deadline the batch loop compares
/// against.  A non-normalised input is `EINVAL`; a zero input stays zero so the
/// first datagram always ends the batch.
///
/// `timespec64_add_safe()` clamps a saturated sum to zero, which is reproduced
/// here instead of wrapping into a bogus deadline.
pub const fn absolute_deadline(now: Timespec64, timeout: Timespec64) -> Option<Timespec64> {
    if !timeout.is_valid() {
        return None;
    }
    if timeout.is_zero() {
        return Some(Timespec64::ZERO);
    }
    let mut sec = (now.sec as u64).wrapping_add(timeout.sec as u64) as i64;
    let mut nsec = now.nsec + timeout.nsec;
    // set_normalized_timespec64(): the summed nanoseconds can reach almost
    // 2 * NSEC_PER_SEC, so carry at most once.
    if nsec >= NSEC_PER_SEC {
        nsec -= NSEC_PER_SEC;
        sec += 1;
    }
    if sec < 0 {
        return Some(Timespec64::ZERO);
    }
    Some(Timespec64::new(sec, nsec))
}

/// `timespec64_sub()` as `do_recvmmsg()` uses it: the time left in the batch.
/// A deadline that has already passed yields [`Timespec64::ZERO`].
pub const fn remaining_timeout(deadline: Timespec64, now: Timespec64) -> Timespec64 {
    let mut sec = deadline.sec - now.sec;
    let mut nsec = deadline.nsec - now.nsec;
    while nsec < 0 {
        nsec += NSEC_PER_SEC;
        sec -= 1;
    }
    if sec < 0 {
        Timespec64::ZERO
    } else {
        Timespec64::new(sec, nsec)
    }
}

/// What `do_recvmmsg()` does after storing the remaining timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchDeadline {
    /// Store the remaining interval and receive the next datagram.
    Continue(Timespec64),
    /// Store the remaining interval and stop the batch.
    Stop(Timespec64),
}

/// `net/socket.c:do_recvmmsg()` recomputes the timeout only *after* a datagram
/// was received, so a relative timeout bounds the gap between datagrams and
/// never the blocking receive itself.  A zero remaining interval stops the
/// batch and is still written back to userspace.
pub const fn batch_deadline(deadline: Timespec64, now: Timespec64) -> BatchDeadline {
    let remaining = remaining_timeout(deadline, now);
    if remaining.is_zero() {
        BatchDeadline::Stop(remaining)
    } else {
        BatchDeadline::Continue(remaining)
    }
}

/// The outcome of one transport receive attempt, as `MSG_WAITALL` observes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveStep {
    /// The transport copied `bytes` octets.
    Copied(usize),
    /// The peer closed and no buffered octet remains.
    Eof,
    /// No octet is available now: `EAGAIN`, `SO_RCVTIMEO` expiry, or a signal.
    Interrupted,
    /// The connection failed.
    Failed,
}

/// `net/ipv4/tcp.c:tcp_recvmsg_locked()` computes
/// `target = sock_rcvlowat(sk, flags & MSG_WAITALL, len)`, which is the whole
/// request under `MSG_WAITALL`, and keeps copying until the target, a FIN, an
/// error, a signal, or a non-blocking/timeout exit.  Once any octet has been
/// copied every one of those exits returns that short count instead of the
/// error, so only a step that made progress continues the loop.
pub const fn waitall_continues(step: ReceiveStep, copied: usize, requested: usize) -> bool {
    match step {
        ReceiveStep::Copied(bytes) => bytes != 0 && copied < requested,
        ReceiveStep::Eof | ReceiveStep::Interrupted | ReceiveStep::Failed => false,
    }
}

/// `MSG_WAITALL` is defined only for a byte stream: every datagram protocol in
/// Linux ignores it and returns one record.  A zero-length request has nothing
/// to complete, and a `MSG_PEEK` receive cannot make progress towards the
/// target because it does not consume octets.
pub const fn waitall_applies(stream: bool, peek: bool, requested: usize) -> bool {
    stream && !peek && requested != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_bits_match_x86_64_linux_uapi() {
        assert_eq!(
            MSG_INTERNAL_SENDMSG_FLAGS,
            MSG_SPLICE_PAGES | MSG_SENDPAGE_NOPOLICY | MSG_SENDPAGE_DECRYPTED | MSG_NO_SHARED_FRAGS
        );
        assert_eq!(MSG_INTERNAL_SENDMSG_FLAGS, 0x0819_0000);
        assert_eq!(MSG_MORE, 0x8000);
        assert_eq!(MSG_DONTROUTE, 0x4);
        assert_eq!(MSG_CONFIRM, 0x800);
        assert_eq!(MSG_WAITFORONE, 0x1_0000);
        assert_eq!(MSG_CMSG_COMPAT, 0x8000_0000);
    }

    #[test]
    fn compat_sendmsg_flag_is_rejected_but_ordinary_flags_are_not() {
        assert_eq!(compat_flag_errno(MSG_CMSG_COMPAT), Some(22));
        assert_eq!(compat_flag_errno(MSG_CMSG_COMPAT | MSG_DONTWAIT), Some(22));
        assert_eq!(
            compat_flag_errno(MSG_MORE | MSG_DONTROUTE | MSG_CONFIRM),
            None
        );
        assert_eq!(compat_flag_errno(0), None);
    }

    #[test]
    fn internal_sendmsg_flags_are_cleared_before_the_protocol() {
        assert_eq!(
            strip_internal_sendmsg_flags(MSG_MORE | MSG_SPLICE_PAGES),
            MSG_MORE
        );
        assert_eq!(strip_internal_sendmsg_flags(MSG_WAITFORONE), 0);
        assert_eq!(strip_internal_sendmsg_flags(MSG_NOSIGNAL), MSG_NOSIGNAL);
    }

    #[test]
    fn timeout_validation_matches_timespec64_valid() {
        assert!(Timespec64::new(0, 0).is_valid());
        assert!(Timespec64::new(0, NSEC_PER_SEC - 1).is_valid());
        assert!(!Timespec64::new(0, NSEC_PER_SEC).is_valid());
        assert!(!Timespec64::new(0, -1).is_valid());
        assert!(!Timespec64::new(-1, 0).is_valid());
    }

    #[test]
    fn absolute_deadline_adds_and_normalises() {
        let now = Timespec64::new(1_000, 900_000_000);
        assert_eq!(
            absolute_deadline(now, Timespec64::new(2, 200_000_000)),
            Some(Timespec64::new(1_003, 100_000_000))
        );
        // A non-normalised request never reaches the loop.
        assert_eq!(
            absolute_deadline(now, Timespec64::new(0, NSEC_PER_SEC)),
            None
        );
        assert_eq!(absolute_deadline(now, Timespec64::new(-1, 0)), None);
    }

    #[test]
    fn zero_timeout_stays_zero_and_expires_immediately() {
        let now = Timespec64::new(5_000, 123);
        assert_eq!(
            absolute_deadline(now, Timespec64::ZERO),
            Some(Timespec64::ZERO)
        );
        assert_eq!(
            batch_deadline(Timespec64::ZERO, now),
            BatchDeadline::Stop(Timespec64::ZERO)
        );
    }

    #[test]
    fn batch_deadline_reports_remaining_time_and_stops_when_expired() {
        let deadline = Timespec64::new(100, 500_000_000);
        assert_eq!(
            batch_deadline(deadline, Timespec64::new(99, 0)),
            BatchDeadline::Continue(Timespec64::new(1, 500_000_000))
        );
        // Borrowing across the second boundary must stay normalised.
        assert_eq!(
            batch_deadline(Timespec64::new(101, 100_000_000), Timespec64::new(100, 750_000_000)),
            BatchDeadline::Continue(Timespec64::new(0, 350_000_000))
        );
        // A deadline that already passed inside the same second normalises to a
        // negative second, which `do_recvmmsg()` stores as zero and stops on.
        assert_eq!(
            batch_deadline(Timespec64::new(100, 500_000_000), Timespec64::new(100, 750_000_000)),
            BatchDeadline::Stop(Timespec64::ZERO)
        );
        assert_eq!(
            batch_deadline(deadline, Timespec64::new(101, 0)),
            BatchDeadline::Stop(Timespec64::ZERO)
        );
        // Exactly at the deadline the batch stops and stores zero.
        assert_eq!(
            batch_deadline(deadline, deadline),
            BatchDeadline::Stop(Timespec64::ZERO)
        );
    }

    #[test]
    fn saturated_deadline_clamps_to_zero_instead_of_wrapping() {
        let now = Timespec64::new(i64::MAX, 0);
        assert_eq!(
            absolute_deadline(now, Timespec64::new(i64::MAX, 0)),
            Some(Timespec64::ZERO)
        );
    }

    #[test]
    fn waitall_continues_only_while_a_stream_makes_progress() {
        assert!(waitall_continues(ReceiveStep::Copied(4), 4, 10));
        assert!(!waitall_continues(ReceiveStep::Copied(10), 10, 10));
        assert!(!waitall_continues(ReceiveStep::Eof, 4, 10));
        assert!(!waitall_continues(ReceiveStep::Interrupted, 4, 10));
        assert!(!waitall_continues(ReceiveStep::Failed, 4, 10));
        // A transport that reports no progress must not spin.
        assert!(!waitall_continues(ReceiveStep::Copied(0), 4, 10));
    }

    #[test]
    fn waitall_applies_only_to_a_non_peeking_byte_stream_request() {
        assert!(waitall_applies(true, false, 16));
        assert!(!waitall_applies(false, false, 16));
        assert!(!waitall_applies(true, true, 16));
        assert!(!waitall_applies(true, false, 0));
    }
}
