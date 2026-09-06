/// Errno categories an embedding adapter can map without importing an errno
/// crate or making this policy leaf depend on a syscall layer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(i32)]
pub enum ErrnoClass {
    /// `EINVAL`: malformed ABI values or restart-gate policy violations.
    InvalidArgument = 22,
    /// `EPERM`: the registration signature does not match an active one.
    PermissionDenied = 1,
    /// `EBUSY`: a registration or lifecycle transaction is already active.
    Busy = 16,
    /// `EFAULT`: an adapter failed to access user memory.
    Fault = 14,
    /// `EAGAIN`: a legacy caller attempted to finalize a stale plan.
    Stale = 11,
    /// `EOVERFLOW`: a non-wrapping policy counter was exhausted.
    Overflow = 75,
}

impl ErrnoClass {
    /// Linux numeric errno value for adapters that use integers.
    pub const fn value(self) -> i32 {
        self as i32
    }

    /// Linux spelling for `EINVAL`.
    pub const EINVAL: Self = Self::InvalidArgument;
    /// Linux spelling for `EPERM`.
    pub const EPERM: Self = Self::PermissionDenied;
    /// Linux spelling for `EBUSY`.
    pub const EBUSY: Self = Self::Busy;
    /// Linux spelling for `EFAULT`.
    pub const EFAULT: Self = Self::Fault;
    /// Linux spelling for `EAGAIN`.
    pub const EAGAIN: Self = Self::Stale;
    /// Linux spelling for `EOVERFLOW`.
    pub const EOVERFLOW: Self = Self::Overflow;
}

/// Alias emphasizing that these values are errno-compatible classifications,
/// not the concrete errno type owned by a syscall adapter.
pub type RseqErrno = ErrnoClass;

/// Stable pure-policy failures returned by `thekernel-linux-rseq`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum RseqError {
    /// A null area pointer was passed.  Registration validation intentionally
    /// does not produce this error: user access must classify it as `EFAULT`.
    InvalidAreaAddress,
    /// The registered area address was not aligned to the v6.6 ABI alignment.
    InvalidAlignment,
    /// The supplied area length was shorter than the 32-byte core ABI.
    InvalidLength,
    /// Registration flags contained a bit outside the operation profile.
    InvalidRegistrationFlags,
    /// The active area flags are unsupported in an in-critical restart.
    InvalidAreaFlags,
    /// A registration already exists with the exact same address, length, and
    /// signature.
    AlreadyRegistered,
    /// No registration exists to unregister.
    NotRegistered,
    /// Address or length did not identify the active registration.
    RegistrationMismatch,
    /// The supplied signature did not identify the active registration.
    SignatureMismatch,
    /// The descriptor address was null.
    InvalidDescriptorAddress,
    /// The copied descriptor could not be obtained from user memory.
    DescriptorReadFault,
    /// The descriptor version was not the Linux v6.6 version zero.
    InvalidVersion,
    /// The descriptor flags contained an unsupported bit in an active restart.
    InvalidFlags,
    /// `start_ip + post_commit_offset` overflowed.
    AddressOverflow,
    /// A descriptor pointer or code address was outside the exclusive user
    /// address limit proof supplied by the adapter.  The descriptor's byte
    /// span is intentionally left to the adapter's usercopy/EFAULT path.
    AddressOutOfRange,
    /// The abort target fell inside the critical-section instruction range.
    AbortInCriticalSection,
    /// `abort_ip - sizeof(u32)` underflowed while deriving the signature word.
    /// This follows the adapter's user-fault path and maps to `EFAULT`.
    SignatureAddressUnderflow,
    /// A supplied signature address was not the word immediately preceding
    /// `abort_ip`.
    SignatureAddressMismatch,
    /// The active `rseq_cs` address did not identify the supplied descriptor.
    ActiveDescriptorMismatch,
    /// The actual abort signature did not match the registration signature.
    /// Linux treats this as a restart `EINVAL`, not registration `EPERM`.
    RestartSignatureMismatch,
    /// A legacy prepare plan was committed after its state changed.
    StalePlan,
    /// The state could not advance to another non-wrapping epoch.
    EpochExhausted,
    /// A pending-event mask contained an unknown bit.
    InvalidEventFlags,
    /// A non-wrapping resume revision was exhausted.
    RevisionExhausted,
    /// A registration/resume/exec transaction is already in flight.
    OperationInProgress,
    /// The area reported the Linux uninitialized CPU sentinel.
    CpuIdUninitialized,
    /// The area reported the Linux registration-failed CPU sentinel.
    CpuIdRegistrationFailed,
    /// The saved and current CPU values did not match.
    CpuIdMismatch,
    /// An expected CPU value was itself a reserved sentinel.
    InvalidCpuId,
}

impl RseqError {
    /// Returns a syscall-facing category without owning an errno type.
    pub const fn errno(self) -> ErrnoClass {
        match self {
            Self::AlreadyRegistered | Self::OperationInProgress => ErrnoClass::Busy,
            Self::SignatureMismatch => ErrnoClass::PermissionDenied,
            Self::StalePlan => ErrnoClass::Stale,
            Self::EpochExhausted | Self::RevisionExhausted => ErrnoClass::Overflow,
            Self::InvalidAreaAddress
            | Self::DescriptorReadFault
            | Self::SignatureAddressUnderflow => ErrnoClass::Fault,
            Self::InvalidAlignment
            | Self::InvalidLength
            | Self::InvalidRegistrationFlags
            | Self::InvalidAreaFlags
            | Self::NotRegistered
            | Self::RegistrationMismatch
            | Self::InvalidDescriptorAddress
            | Self::InvalidVersion
            | Self::InvalidFlags
            | Self::AddressOverflow
            | Self::AddressOutOfRange
            | Self::AbortInCriticalSection
            | Self::SignatureAddressMismatch
            | Self::ActiveDescriptorMismatch
            | Self::RestartSignatureMismatch
            | Self::InvalidEventFlags
            | Self::CpuIdUninitialized
            | Self::CpuIdRegistrationFailed
            | Self::CpuIdMismatch
            | Self::InvalidCpuId => ErrnoClass::InvalidArgument,
        }
    }

    /// Returns the numeric errno category for adapters that use integers.
    pub const fn errno_value(self) -> i32 {
        self.errno().value()
    }

    /// Whether the error belongs to Linux's invalid-argument class.
    pub const fn is_invalid_argument(self) -> bool {
        matches!(self.errno(), ErrnoClass::InvalidArgument)
    }
}
