use crate::{PacketError, PacketSocketType};

/// Linux `MSG_PEEK` bit accepted by the first-stage decision core.
pub const MSG_PEEK: u32 = 0x02;
/// Linux `MSG_TRUNC` bit accepted by the first-stage decision core.
pub const MSG_TRUNC: u32 = 0x20;

/// Strictly decoded receive flags owned by this first-stage profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceiveFlags(u32);

impl ReceiveFlags {
    /// No special receive behavior.
    pub const EMPTY: Self = Self(0);
    /// Inspect without consuming the queued packet.
    pub const PEEK: Self = Self(MSG_PEEK);
    /// Return the full captured length even when the copy buffer is shorter.
    pub const TRUNC: Self = Self(MSG_TRUNC);
    /// Complete supported flag set.
    pub const SUPPORTED: Self = Self(MSG_PEEK | MSG_TRUNC);

    /// Strictly decodes copied receive flags.
    ///
    /// Nonblocking and signal interruption are adapter-owned wait decisions,
    /// so this core rejects rather than silently ignores their raw bits.
    pub const fn from_bits(bits: u32) -> Result<Self, PacketError> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(PacketError::UnsupportedReceiveFlags)
        }
    }

    /// Returns Linux-compatible flag bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether every requested flag is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Combines two already validated flags.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Validated complete-frame and network-header geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLayout {
    frame_len: usize,
    network_offset: usize,
}

impl FrameLayout {
    /// Validates that the network header begins within the complete frame.
    pub const fn new(frame_len: usize, network_offset: usize) -> Result<Self, PacketError> {
        if network_offset > frame_len {
            return Err(PacketError::InvalidFrameLayout);
        }
        Ok(Self {
            frame_len,
            network_offset,
        })
    }

    /// Complete link-layer frame length.
    pub const fn frame_len(self) -> usize {
        self.frame_len
    }

    /// Offset of the cooked payload's network header.
    pub const fn network_offset(self) -> usize {
        self.network_offset
    }

    /// Selects the complete RAW or DGRAM view before filter truncation.
    pub const fn full_view(self, socket_type: PacketSocketType) -> PacketView {
        let payload_offset = match socket_type {
            PacketSocketType::Raw => 0,
            PacketSocketType::Datagram => self.network_offset,
        };
        let original_len = self.frame_len - payload_offset;
        PacketView {
            payload_offset,
            original_len,
            captured_len: original_len,
        }
    }

    /// Selects a RAW or DGRAM view and validates its post-filter snap length.
    pub const fn captured_view(
        self,
        socket_type: PacketSocketType,
        captured_len: usize,
    ) -> Result<PacketView, PacketError> {
        let full = self.full_view(socket_type);
        if captured_len > full.original_len {
            return Err(PacketError::InvalidCapturedLength);
        }
        Ok(PacketView {
            captured_len,
            ..full
        })
    }
}

/// One admitted packet as visible to a RAW or cooked DGRAM socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PacketView {
    payload_offset: usize,
    original_len: usize,
    captured_len: usize,
}

impl PacketView {
    /// Byte offset of the first visible byte in the complete link frame.
    pub const fn payload_offset(self) -> usize {
        self.payload_offset
    }

    /// Length before socket-filter snap truncation.
    pub const fn original_len(self) -> usize {
        self.original_len
    }

    /// Length retained after socket-filter snap truncation.
    pub const fn captured_len(self) -> usize {
        self.captured_len
    }

    /// Produces the pure copy/return/queue decision for one receive call.
    ///
    /// The adapter must apply the queue disposition before usercopy. Ordinary
    /// receive claims the record and never requeues it after a copy fault;
    /// `MSG_PEEK` leaves the record queued even when copying fails.
    pub const fn receive_decision(
        self,
        copy_buffer_len: usize,
        flags: ReceiveFlags,
    ) -> ReceiveDecision {
        let copy_len = if copy_buffer_len < self.captured_len {
            copy_buffer_len
        } else {
            self.captured_len
        };
        let returned_len = if flags.contains(ReceiveFlags::TRUNC) {
            self.captured_len
        } else {
            copy_len
        };
        ReceiveDecision {
            payload_offset: self.payload_offset,
            original_len: self.original_len,
            captured_len: self.captured_len,
            copy_len,
            returned_len,
            message_truncated: copy_len < self.captured_len,
            queue_disposition: if flags.contains(ReceiveFlags::PEEK) {
                QueueDisposition::Retain
            } else {
                QueueDisposition::Consume
            },
        }
    }
}

/// Whether an ordinary queued packet remains after a receive operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDisposition {
    /// `MSG_PEEK`: never claim the packet, including when usercopy fails.
    Retain,
    /// Ordinary receive: claim before usercopy and do not requeue on failure.
    Consume,
}

impl QueueDisposition {
    /// Returns whether the queue adapter must claim the record before usercopy.
    pub const fn claims_before_copy(self) -> bool {
        matches!(self, Self::Consume)
    }
}

/// Pure receive result used by usercopy and queue adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReceiveDecision {
    payload_offset: usize,
    original_len: usize,
    captured_len: usize,
    copy_len: usize,
    returned_len: usize,
    message_truncated: bool,
    queue_disposition: QueueDisposition,
}

impl ReceiveDecision {
    /// Byte offset in the complete link frame at which copying begins.
    pub const fn payload_offset(self) -> usize {
        self.payload_offset
    }

    /// Socket-visible length before filter snap truncation.
    pub const fn original_len(self) -> usize {
        self.original_len
    }

    /// Socket-visible length retained after filter snap truncation.
    pub const fn captured_len(self) -> usize {
        self.captured_len
    }

    /// Bytes copied into the caller's buffer.
    pub const fn copy_len(self) -> usize {
        self.copy_len
    }

    /// Successful syscall result before adapter conversion to `ssize_t`.
    pub const fn returned_len(self) -> usize {
        self.returned_len
    }

    /// Whether output `msg_flags` must include `MSG_TRUNC`.
    pub const fn message_truncated(self) -> bool {
        self.message_truncated
    }

    /// Whether the queue adapter retains or claims the packet before usercopy.
    pub const fn queue_disposition(self) -> QueueDisposition {
        self.queue_disposition
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_datagram_views_choose_different_starts() {
        let layout = FrameLayout::new(100, 14).unwrap();
        let raw = layout.full_view(PacketSocketType::Raw);
        assert_eq!(raw.payload_offset(), 0);
        assert_eq!(raw.original_len(), 100);

        let cooked = layout.full_view(PacketSocketType::Datagram);
        assert_eq!(cooked.payload_offset(), 14);
        assert_eq!(cooked.original_len(), 86);
        assert_eq!(
            layout.captured_view(PacketSocketType::Datagram, 87),
            Err(PacketError::InvalidCapturedLength)
        );
    }

    #[test]
    fn peek_and_trunc_are_independent_decisions() {
        let view = FrameLayout::new(100, 14)
            .unwrap()
            .captured_view(PacketSocketType::Raw, 80)
            .unwrap();
        let ordinary = view.receive_decision(32, ReceiveFlags::EMPTY);
        assert_eq!(ordinary.copy_len(), 32);
        assert_eq!(ordinary.returned_len(), 32);
        assert!(ordinary.message_truncated());
        assert_eq!(ordinary.queue_disposition(), QueueDisposition::Consume);
        assert!(ordinary.queue_disposition().claims_before_copy());

        let flags = ReceiveFlags::PEEK.union(ReceiveFlags::TRUNC);
        let peek = view.receive_decision(32, flags);
        assert_eq!(peek.copy_len(), 32);
        assert_eq!(peek.returned_len(), 80);
        assert_eq!(peek.queue_disposition(), QueueDisposition::Retain);
        assert!(!peek.queue_disposition().claims_before_copy());
        assert_eq!(peek.original_len(), 100);
    }

    #[test]
    fn unsupported_flags_fail_before_queue_or_usercopy_work() {
        assert_eq!(
            ReceiveFlags::from_bits(MSG_PEEK | (1 << 30)),
            Err(PacketError::UnsupportedReceiveFlags)
        );
    }
}
