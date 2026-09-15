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
/// Linux `MSG_PEEK`.
pub const MSG_PEEK: u32 = 0x2;
/// Linux `MSG_DONTROUTE` (also `MSG_TRYHARD`).
pub const MSG_DONTROUTE: u32 = 0x4;
/// Linux `MSG_CTRUNC` (receive output only).
pub const MSG_CTRUNC: u32 = 0x8;
/// Linux `MSG_PROBE`.
pub const MSG_PROBE: u32 = 0x10;
/// Linux `MSG_TRUNC`.
pub const MSG_TRUNC: u32 = 0x20;
/// Linux `MSG_DONTWAIT`.
pub const MSG_DONTWAIT: u32 = 0x40;
/// Linux `MSG_EOR`.
pub const MSG_EOR: u32 = 0x80;
/// Linux `MSG_WAITALL`.
pub const MSG_WAITALL: u32 = 0x100;
/// Linux `MSG_ERRQUEUE`.
pub const MSG_ERRQUEUE: u32 = 0x2000;
/// Linux `MSG_BATCH`: `do_sendmmsg()` sets it on every message but the last
/// (`net/socket.c:2813`).
pub const MSG_BATCH: u32 = 0x4_0000;
/// Linux `MSG_ZEROCOPY`.
pub const MSG_ZEROCOPY: u32 = 0x0400_0000;
/// Linux `MSG_FASTOPEN`.
pub const MSG_FASTOPEN: u32 = 0x2000_0000;
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

/// The receive flags `net/packet/af_packet.c:packet_recvmsg()` admits.
///
/// Every other protocol passes `flags` to its own receive loop and ignores the
/// bits it does not use, but AF_PACKET alone tests an allow-list before doing
/// anything else:
///
/// ```c
/// 	err = -EINVAL;
/// 	if (flags & ~(MSG_PEEK|MSG_DONTWAIT|MSG_TRUNC|MSG_CMSG_COMPAT|MSG_ERRQUEUE))
/// 		goto out;
/// ```
///
/// so `recv(packet_fd, …, MSG_WAITALL)`, `MSG_OOB` and `MSG_CMSG_CLOEXEC` are
/// `EINVAL` for this family and a legal no-op for the others.
pub const PACKET_RECVMSG_FLAGS: u32 =
    MSG_PEEK | MSG_DONTWAIT | MSG_TRUNC | MSG_CMSG_COMPAT | MSG_ERRQUEUE;

/// `packet_recvmsg()`'s allow-list test: `EINVAL` for any bit it does not
/// consume.
pub const fn packet_recvmsg_flag_errno(flags: u32) -> Option<i32> {
    if flags & !PACKET_RECVMSG_FLAGS != 0 {
        Some(22)
    } else {
        None
    }
}

/// The protocol one `MSG_OOB`-bearing message reaches.
///
/// Linux has no socket-layer `MSG_OOB` policy: `__sys_sendto()` clears only
/// `MSG_INTERNAL_SENDMSG_FLAGS` and hands the rest to the protocol
/// (`net/socket.c:2248-2252`), and `__sys_recvfrom()`/`____sys_recvmsg()` do the
/// same (`:2277-2290`, `:2895-2904`).  Every answer below belongs to the
/// transport, and only the transport's identity selects it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MsgOobTransport {
    /// `net/ipv4/tcp.c`
    Tcp,
    /// `net/ipv4/udp.c`
    Udp,
    /// `net/ipv4/raw.c`
    Raw,
    /// `net/netlink/af_netlink.c`
    Netlink,
    /// `net/packet/af_packet.c`
    Packet,
    /// `net/unix/af_unix.c:unix_stream_sendmsg`/`unix_stream_read_generic`
    UnixStream,
    /// `net/unix/af_unix.c:unix_dgram_*`, also reached by `SOCK_SEQPACKET`,
    /// whose `unix_seqpacket_sendmsg`/`unix_seqpacket_recvmsg` delegate to it
    UnixDatagram,
    /// `net/sctp/socket.c`
    Sctp,
    /// `net/vmw_vsock/af_vsock.c`
    Vsock,
}

/// Which half of a message transfer carries the flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageDirection {
    /// `sendto`/`sendmsg`/`sendmmsg`.
    Send,
    /// `recvfrom`/`recvmsg`/`recvmmsg`.
    Receive,
}

/// `tcp_recv_urg()`'s "no URG data to read" answer, `net/ipv4/tcp.c:1480-1483`.
pub const NO_URGENT_DATA_ERRNO: i32 = 22;
/// `udp_sendmsg()`'s "Mirror BSD error message compatibility" answer,
/// `net/ipv4/udp.c:1259-1261`.
pub const OOB_NOT_SUPPORTED_ERRNO: i32 = 95;

/// Linux's per-protocol `MSG_OOB` answer.
///
/// `None` means the protocol consumes or ignores the bit and the transfer
/// proceeds; `Some(errno)` is the exact failure that protocol raises.
///
/// * TCP consumes it on send — `tcp_mark_urg()` runs inside
///   `tcp_sendmsg_locked()` and no frame on that path rejects the bit
///   (`net/ipv4/tcp.c:715-718`), so the octets are sent.  On receive the bit is
///   diverted before the copy loop,
///
///   ```c
///   	/* Urgent data needs to be handled specially. */
///   	if (flags & MSG_OOB)
///   		goto recv_urg;
///   ```
///
///   (`net/ipv4/tcp.c:2679-2681`), and `tcp_recv_urg()` answers `-EINVAL`
///   whenever no urgent byte is pending:
///
///   ```c
///   	if (sock_flag(sk, SOCK_URGINLINE) || !tp->urg_data ||
///   	    tp->urg_data == TCP_URG_READ)
///   		return -EINVAL;	/* Yes this is right ! */
///   ```
///
///   (`net/ipv4/tcp.c:1480-1483`).
/// * UDP rejects it on send (`net/ipv4/udp.c:1259-1261`) and never tests it on
///   receive, so `udp_recvmsg()` delivers the datagram normally.
/// * RAW rejects it in both directions (`net/ipv4/raw.c:517`, `:758`).
/// * Netlink rejects it in both directions (`net/netlink/af_netlink.c:1830`,
///   `:1917`).
/// * AF_UNIX stream consumes it when the kernel builds with `CONFIG_AF_UNIX_OOB`:
///   `unix_stream_sendmsg()` reserves the last octet for `queue_oob()`
///   (`net/unix/af_unix.c:2392-2400`, `:2496-2502`) and
///   `unix_stream_read_generic()` routes the receive bit to
///   `unix_stream_recv_urg()` (`:2929-2934`), which answers `-EINVAL` while
///   `u->oob_skb` is NULL (`:2776-2782`).  Without that option both directions
///   are `-EOPNOTSUPP`.  The pinned v7.2.3 oracle sets `CONFIG_AF_UNIX_OOB=y`.
/// * AF_UNIX datagram and seqpacket always answer `-EOPNOTSUPP`
///   (`net/unix/af_unix.c:2099-2102`, `:2392-2395`, `:2572-2576`).
/// * SCTP never tests the bit, so it is consumed like any other unread flag.
/// * Vsock rejects it in both directions (`net/vmw_vsock/af_vsock.c:2195-2197`,
///   `:2571-2573`).
/// * AF_PACKET never tests the bit on send, and its receive path refuses it
///   through the `packet_recvmsg()` allow-list that
///   [`packet_recvmsg_flag_errno`] already reports, so both directions are left
///   to that call.
pub const fn msg_oob_errno(
    transport: MsgOobTransport,
    direction: MessageDirection,
    unix_oob_supported: bool,
) -> Option<i32> {
    match (transport, direction) {
        (MsgOobTransport::Tcp, MessageDirection::Send) => None,
        (MsgOobTransport::Tcp, MessageDirection::Receive) => Some(NO_URGENT_DATA_ERRNO),
        (MsgOobTransport::Udp, MessageDirection::Send) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::Udp, MessageDirection::Receive) => None,
        (MsgOobTransport::Raw, _) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::Netlink, _) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::UnixStream, MessageDirection::Send) if unix_oob_supported => None,
        (MsgOobTransport::UnixStream, MessageDirection::Receive) if unix_oob_supported => {
            Some(NO_URGENT_DATA_ERRNO)
        }
        (MsgOobTransport::UnixStream, _) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::UnixDatagram, _) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::Sctp, _) => None,
        (MsgOobTransport::Vsock, _) => Some(OOB_NOT_SUPPORTED_ERRNO),
        (MsgOobTransport::Packet, _) => None,
    }
}

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

/// `sock_rcvlowat()` (`include/net/sock.h:735-741`): the number of octets a
/// stream receive must have copied before it may stop waiting.
///
/// ```c
/// 	int v = waitall ? len : min_t(int, READ_ONCE(sk->sk_rcvlowat), len);
/// 	return v ?: 1;
/// ```
///
/// `MSG_WAITALL` therefore raises the target to the whole request, while an
/// ordinary receive keeps the socket's low-water mark (one octet unless
/// `SO_RCVLOWAT` says otherwise).  `tcp_recvmsg_locked()` and
/// `unix_stream_read_generic()` both derive their target this way.
pub const fn sock_rcvlowat(waitall: bool, receive_low_water: usize, len: usize) -> usize {
    let target = if waitall {
        len
    } else if receive_low_water < len {
        receive_low_water
    } else {
        len
    };
    if target == 0 { 1 } else { target }
}

/// The receive target after the transports' own rules are applied.
///
/// `MSG_WAITALL` is defined only for a byte stream: every datagram protocol
/// ignores it and returns one record, so their target is the single octet that
/// makes the completion loop stop immediately.
///
/// Whether a `MSG_PEEK` receive counts towards the target is likewise the
/// transport's own decision:
///
/// * `net/ipv4/tcp.c:tcp_recvmsg_locked()` counts every octet it copied,
///   including a peeked one — `copied += used; len -= used;`
///   (`:2874-2875`) runs before the `if (flags & MSG_PEEK)` split at `:2877` —
///   and the peek cursor itself advances (`peek_seq = tp->copied_seq +
///   peek_offset` at `:2701-2702`).  A `recv(MSG_WAITALL|MSG_PEEK)` therefore
///   blocks until the whole request is readable and then returns it without
///   consuming anything.
/// * `net/unix/af_unix.c:unix_stream_read_generic()` does not: the `MSG_PEEK`
///   arm leaves the copy loop at `:3068-3070` (`if (skb) goto again; … break;`)
///   when the receive queue runs out, so an AF_UNIX peek returns the available
///   prefix at once.
pub const fn receive_wait_target(
    stream: bool,
    peek: bool,
    peek_advances_cursor: bool,
    waitall: bool,
    receive_low_water: usize,
    requested: usize,
) -> usize {
    if !stream || (peek && !peek_advances_cursor) {
        return 1;
    }
    sock_rcvlowat(waitall, receive_low_water, requested)
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
    fn sock_rcvlowat_raises_the_target_only_for_waitall() {
        // `waitall ? len : min_t(int, sk_rcvlowat, len)`, then `?: 1`.
        assert_eq!(sock_rcvlowat(true, 1, 16), 16);
        assert_eq!(sock_rcvlowat(false, 1, 16), 1);
        assert_eq!(sock_rcvlowat(false, 8, 16), 8);
        assert_eq!(sock_rcvlowat(false, 32, 16), 16);
        assert_eq!(sock_rcvlowat(false, 0, 16), 1);
        assert_eq!(sock_rcvlowat(true, 4, 0), 1);
    }

    #[test]
    fn receive_target_is_reached_only_where_the_transport_advances_its_peek_cursor() {
        // TCP: `tcp_recvmsg_locked()` counts peeked octets (net/ipv4/tcp.c:2874-2877).
        assert_eq!(receive_wait_target(true, false, true, true, 1, 16), 16);
        assert_eq!(receive_wait_target(true, true, true, true, 1, 16), 16);
        // AF_UNIX stream: the peek arm leaves the loop before the target wait
        // (net/unix/af_unix.c:3068-3070), so a peek gets one pass.
        assert_eq!(receive_wait_target(true, false, false, true, 1, 16), 16);
        assert_eq!(receive_wait_target(true, true, false, true, 1, 16), 1);
        // MSG_WAITALL is a byte-stream rule; datagram transports ignore it.
        assert_eq!(receive_wait_target(false, false, true, true, 1, 16), 1);
        assert_eq!(receive_wait_target(false, true, true, true, 1, 16), 1);
        // A plain stream receive keeps the low-water target of one octet.
        assert_eq!(receive_wait_target(true, false, true, false, 1, 16), 1);
        // A zero-length request has nothing to complete.
        assert_eq!(receive_wait_target(true, false, true, true, 1, 0), 1);
    }

    #[test]
    fn only_packet_recvmsg_rejects_receive_flags_it_does_not_consume() {
        // `net/packet/af_packet.c:packet_recvmsg()`'s allow-list.
        assert_eq!(packet_recvmsg_flag_errno(MSG_PEEK | MSG_DONTWAIT), None);
        assert_eq!(packet_recvmsg_flag_errno(MSG_TRUNC | MSG_ERRQUEUE), None);
        assert_eq!(packet_recvmsg_flag_errno(MSG_CMSG_COMPAT), None);
        // Every other protocol ignores these bits instead of failing.
        assert_eq!(packet_recvmsg_flag_errno(MSG_WAITALL), Some(22));
        assert_eq!(packet_recvmsg_flag_errno(MSG_OOB), Some(22));
        assert_eq!(packet_recvmsg_flag_errno(MSG_CMSG_CLOEXEC), Some(22));
    }

    #[test]
    fn msg_oob_is_a_per_transport_answer_not_a_socket_layer_refusal() {
        use MessageDirection::{Receive, Send};
        use MsgOobTransport::{
            Netlink, Packet, Raw, Sctp, Tcp, Udp, UnixDatagram, UnixStream, Vsock,
        };

        // TCP consumes MSG_OOB on send (`net/ipv4/tcp.c:715-718`) and answers
        // `tcp_recv_urg()`'s -EINVAL when no urgent byte is pending
        // (`net/ipv4/tcp.c:1480-1483`).
        assert_eq!(msg_oob_errno(Tcp, Send, false), None);
        assert_eq!(msg_oob_errno(Tcp, Receive, false), Some(22));

        // UDP mirrors the BSD EOPNOTSUPP on send only; `udp_recvmsg()` never
        // tests the bit (`net/ipv4/udp.c:1259-1261`).
        assert_eq!(msg_oob_errno(Udp, Send, false), Some(95));
        assert_eq!(msg_oob_errno(Udp, Receive, false), None);

        // RAW and netlink reject both directions (`net/ipv4/raw.c:517`, `:758`;
        // `net/netlink/af_netlink.c:1830`, `:1917`).
        assert_eq!(msg_oob_errno(Raw, Send, false), Some(95));
        assert_eq!(msg_oob_errno(Raw, Receive, false), Some(95));
        assert_eq!(msg_oob_errno(Netlink, Send, false), Some(95));
        assert_eq!(msg_oob_errno(Netlink, Receive, false), Some(95));

        // AF_UNIX datagram and seqpacket always refuse it
        // (`net/unix/af_unix.c:2099-2102`, `:2572-2576`).
        assert_eq!(msg_oob_errno(UnixDatagram, Send, true), Some(95));
        assert_eq!(msg_oob_errno(UnixDatagram, Receive, true), Some(95));

        // AF_UNIX stream follows CONFIG_AF_UNIX_OOB: on the pinned oracle the
        // send consumes the bit and a receive with nothing queued is -EINVAL
        // (`net/unix/af_unix.c:2392-2400`, `:2776-2782`, `:2929-2934`).
        assert_eq!(msg_oob_errno(UnixStream, Send, true), None);
        assert_eq!(msg_oob_errno(UnixStream, Receive, true), Some(22));
        assert_eq!(msg_oob_errno(UnixStream, Send, false), Some(95));
        assert_eq!(msg_oob_errno(UnixStream, Receive, false), Some(95));

        // SCTP never tests the bit; vsock always refuses it.
        assert_eq!(msg_oob_errno(Sctp, Send, false), None);
        assert_eq!(msg_oob_errno(Sctp, Receive, false), None);
        assert_eq!(msg_oob_errno(Vsock, Send, false), Some(95));
        assert_eq!(msg_oob_errno(Vsock, Receive, false), Some(95));

        // AF_PACKET is answered by `packet_recvmsg_flag_errno()`.
        assert_eq!(msg_oob_errno(Packet, Send, false), None);
        assert_eq!(msg_oob_errno(Packet, Receive, false), None);
    }
}
