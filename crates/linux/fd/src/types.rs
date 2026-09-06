use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

/// Non-negative Linux file-descriptor number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FdNumber(u32);

impl FdNumber {
    /// Creates a descriptor number from its unsigned ABI representation.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// Rejects negative signed ABI values.
    pub const fn from_i32(raw: i32) -> Option<Self> {
        if raw < 0 {
            None
        } else {
            Some(Self(raw as u32))
        }
    }

    /// Returns the numeric descriptor.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns the descriptor as an array index when representable.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Caller-allocated stable identity for one Linux files table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FdTableId(u64);

impl FdTableId {
    /// Creates a nonzero identity.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// Returns the raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Caller-allocated stable identity for one open file description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OfdId(u64);

impl OfdId {
    /// Creates a nonzero identity.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// Returns the raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Caller-allocated stable identity for one epoll instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpollId(u64);

impl EpollId {
    /// Creates a nonzero identity.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// Returns the raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Caller-allocated stable identity for one serialized epoll graph domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EpollGraphId(u64);

impl EpollGraphId {
    /// Creates a nonzero identity.
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    /// Returns the raw identity.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Descriptor-local flags. These are copied by fork and do not belong to the
/// shared open file description.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DescriptorFlags(u8);

impl DescriptorFlags {
    /// No descriptor-local flags.
    pub const EMPTY: Self = Self(0);
    /// Close this descriptor during a successful exec transition.
    pub const CLOSE_ON_EXEC: Self = Self(1);

    /// Returns the crate-owned bit representation.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Returns whether every flag in `other` is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Inserts or removes `flag`.
    pub fn set(&mut self, flag: Self, enabled: bool) {
        if enabled {
            self.0 |= flag.0;
        } else {
            self.0 &= !flag.0;
        }
    }
}

/// Linux poll/epoll interest bits after strict syscall decoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterestMask(u32);

impl InterestMask {
    /// Readable data.
    pub const IN: Self = Self(0x0001);
    /// Priority data.
    pub const PRI: Self = Self(0x0002);
    /// Writable space.
    pub const OUT: Self = Self(0x0004);
    /// Normal readable data.
    pub const READ_NORMAL: Self = Self(0x0040);
    /// Priority/band readable data.
    pub const READ_BAND: Self = Self(0x0080);
    /// Normal writable space.
    pub const WRITE_NORMAL: Self = Self(0x0100);
    /// Priority/band writable space.
    pub const WRITE_BAND: Self = Self(0x0200);
    /// Peer half-close.
    pub const READ_HANGUP: Self = Self(0x2000);

    /// All normal requestable readiness bits supported by the core.
    pub const ALL: Self = Self(
        Self::IN.0
            | Self::PRI.0
            | Self::OUT.0
            | Self::READ_NORMAL.0
            | Self::READ_BAND.0
            | Self::WRITE_NORMAL.0
            | Self::WRITE_BAND.0
            | Self::READ_HANGUP.0,
    );

    /// Strictly decodes normal interest bits.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns raw Linux-compatible normal-interest bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether the mask is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for InterestMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for InterestMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Linux readiness results. Error, hangup, and invalid-descriptor conditions
/// are deliverable even when absent from the normal interest mask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ReadyMask(u32);

impl ReadyMask {
    /// No readiness.
    pub const EMPTY: Self = Self(0);
    /// Readable data.
    pub const IN: Self = Self(0x0001);
    /// Priority data.
    pub const PRI: Self = Self(0x0002);
    /// Writable space.
    pub const OUT: Self = Self(0x0004);
    /// I/O error.
    pub const ERROR: Self = Self(0x0008);
    /// Hangup.
    pub const HANGUP: Self = Self(0x0010);
    /// Invalid descriptor.
    pub const INVALID: Self = Self(0x0020);
    /// Normal readable data.
    pub const READ_NORMAL: Self = Self(0x0040);
    /// Priority/band readable data.
    pub const READ_BAND: Self = Self(0x0080);
    /// Normal writable space.
    pub const WRITE_NORMAL: Self = Self(0x0100);
    /// Priority/band writable space.
    pub const WRITE_BAND: Self = Self(0x0200);
    /// Peer half-close.
    pub const READ_HANGUP: Self = Self(0x2000);

    const UNCONDITIONAL: Self = Self(Self::ERROR.0 | Self::HANGUP.0 | Self::INVALID.0);

    /// Creates a mask while discarding no result bits.
    pub const fn from_bits_retain(bits: u32) -> Self {
        Self(bits)
    }

    /// Returns the raw result bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether the mask is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Filters readiness using Linux's unconditional error/hangup rule.
    pub const fn deliverable(self, interest: InterestMask) -> Self {
        Self(self.0 & (interest.0 | Self::UNCONDITIONAL.0))
    }
}

impl BitOr for ReadyMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ReadyMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for ReadyMask {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for ReadyMask {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for ReadyMask {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0)
    }
}

/// Linux epoll triggering behavior independent of the event mask.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InterestMode {
    /// Edge-triggered delivery instead of level-triggered requeue.
    pub edge: bool,
    /// Disable the interest after one successful delivery until explicit rearm.
    pub one_shot: bool,
    /// Exclusive wake selection for supported wakeup objects.
    pub exclusive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_descriptors_reject_negative_values() {
        assert_eq!(FdNumber::from_i32(-1), None);
        assert_eq!(FdNumber::from_i32(7), Some(FdNumber::new(7)));
    }

    #[test]
    fn readiness_always_reports_error_and_hangup() {
        let ready = ReadyMask::ERROR | ReadyMask::HANGUP | ReadyMask::OUT;
        assert_eq!(
            ready.deliverable(InterestMask::IN),
            ReadyMask::ERROR | ReadyMask::HANGUP
        );
    }

    #[test]
    fn interest_decoder_rejects_result_and_trigger_bits() {
        assert!(InterestMask::from_bits(InterestMask::IN.bits()).is_some());
        assert!(InterestMask::from_bits(ReadyMask::ERROR.bits()).is_none());
        assert!(InterestMask::from_bits(1 << 31).is_none());
    }
}
