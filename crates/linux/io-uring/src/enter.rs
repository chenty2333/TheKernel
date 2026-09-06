use crate::IoUringError;

/// Strictly decoded `io_uring_enter` flags supported by this profile.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EnterFlags(u32);

impl EnterFlags {
    /// Wait until the requested minimum completion count is available.
    pub const GETEVENTS: Self = Self(1 << 0);
    /// Wake an idle `IORING_SETUP_SQPOLL` submission worker.
    pub const SQ_WAKEUP: Self = Self(1 << 1);
    /// Interpret `argp` as [`IoUringGeteventsArg`].
    pub const EXT_ARG: Self = Self(1 << 3);
    /// Wait for a SQPOLL worker to consume the current submission tail.
    pub const SQ_WAIT: Self = Self(1 << 2);
    /// `fd` is a registered-ring index rather than a descriptor number.
    pub const REGISTERED_RING: Self = Self(1 << 4);
    /// Treat the EXT_ARG timeout as an absolute monotonic deadline.
    pub const ABS_TIMER: Self = Self(1 << 5);
    /// `argp` is an index into a registered wait-argument region.
    pub const EXT_ARG_REG: Self = Self(1 << 6);
    /// Do not enter an I/O-wait scheduler state while waiting for CQEs.
    pub const NO_IOWAIT: Self = Self(1 << 7);
    /// Complete v6.18 enter flag set.  Semantic combinations which need
    /// registered owners are checked by the kernel adapter after lookup.
    pub const SUPPORTED: Self = Self(
        Self::GETEVENTS.0
            | Self::SQ_WAKEUP.0
            | Self::SQ_WAIT.0
            | Self::EXT_ARG.0
            | Self::REGISTERED_RING.0
            | Self::ABS_TIMER.0
            | Self::EXT_ARG_REG.0
            | Self::NO_IOWAIT.0,
    );

    /// Rejects unknown modifiers before submission state changes.
    pub const fn from_bits(bits: u32) -> Result<Self, IoUringError> {
        if bits & !Self::SUPPORTED.0 == 0 {
            Ok(Self(bits))
        } else {
            Err(IoUringError::UnsupportedEnterFlags)
        }
    }

    /// Linux-compatible raw enter bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all selected bits are present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Stable copied form of Linux v6.18's 24-byte
/// `io_uring_getevents_arg`.  The ABI layer only decodes integer words; the
/// kernel adapter owns the ordered usercopy of the pointed-to sigset/timespec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoUringGeteventsArg {
    sigmask: u64,
    sigmask_size: u32,
    min_wait_usec: u32,
    timespec: u64,
}

impl IoUringGeteventsArg {
    /// Exact v6.18 UAPI size on x86_64.
    pub const BYTES: usize = 24;

    /// Decodes a completely copied native-endian UAPI record.  The record is
    /// all integer words and has no reserved bits to reject.
    pub const fn from_ne_bytes(bytes: [u8; Self::BYTES]) -> Self {
        Self {
            sigmask: u64::from_ne_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            sigmask_size: u32::from_ne_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            min_wait_usec: u32::from_ne_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            timespec: u64::from_ne_bytes([
                bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22],
                bytes[23],
            ]),
        }
    }

    pub const fn sigmask_address(self) -> u64 {
        self.sigmask
    }
    pub const fn sigmask_size(self) -> u32 {
        self.sigmask_size
    }
    pub const fn min_wait_usec(self) -> u32 {
        self.min_wait_usec
    }
    pub const fn timespec_address(self) -> u64 {
        self.timespec
    }
}

/// Optional legacy signal-mask copyin requested for one enter invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacySignalMask {
    /// No temporary signal mask is requested.
    None,
    /// Copy exactly the consumer-provided native `SignalSet` size from this
    /// userspace address, install it for the wait, and restore on every exit.
    Address(u64),
}

/// `argp` interpretation retained after the outer syscall arguments have been
/// checked.  The extended record itself is value-decoded; its pointed-to data
/// must be copied by the kernel adapter before a wait may outlive user access.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnterArgument {
    Legacy(LegacySignalMask),
    Extended(IoUringGeteventsArg),
}

/// Copied scalar `io_uring_enter` arguments after strict initial decoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnterRequest {
    to_submit: u32,
    minimum_complete: u32,
    flags: EnterFlags,
    argument: EnterArgument,
}

impl EnterRequest {
    /// Validates initial enter flags and retains legacy signal-mask geometry.
    ///
    /// `expected_signal_set_bytes` is supplied by the signal ABI adapter. A
    /// present legacy mask must match it exactly. The returned value does not
    /// install a mask; the adapter must use a restoration guard spanning every
    /// success, interruption, timeout, and error exit.
    pub const fn from_raw(
        to_submit: u32,
        minimum_complete: u32,
        flags: u32,
        signal_mask_address: u64,
        signal_mask_bytes: u64,
        expected_signal_set_bytes: u64,
    ) -> Result<Self, IoUringError> {
        let flags = match EnterFlags::from_bits(flags) {
            Ok(flags) => flags,
            Err(error) => return Err(error),
        };
        if flags.contains(EnterFlags::EXT_ARG) {
            return Err(IoUringError::InvalidSignalMaskArgument);
        }
        let signal_mask = if signal_mask_address == 0 {
            LegacySignalMask::None
        } else {
            if expected_signal_set_bytes == 0 || signal_mask_bytes != expected_signal_set_bytes {
                return Err(IoUringError::InvalidSignalMaskArgument);
            }
            LegacySignalMask::Address(signal_mask_address)
        };
        Ok(Self {
            to_submit,
            minimum_complete,
            flags,
            argument: EnterArgument::Legacy(signal_mask),
        })
    }

    /// Constructs a request after the kernel adapter has copied the exact
    /// v6.18 extended-argument record.  `EXT_ARG` is mandatory here so a
    /// caller cannot reinterpret a legacy sigset pointer as this record.
    pub const fn from_extended(
        to_submit: u32,
        minimum_complete: u32,
        flags: u32,
        argument: IoUringGeteventsArg,
    ) -> Result<Self, IoUringError> {
        let flags = match EnterFlags::from_bits(flags) {
            Ok(flags) => flags,
            Err(error) => return Err(error),
        };
        if !flags.contains(EnterFlags::EXT_ARG) {
            return Err(IoUringError::InvalidSignalMaskArgument);
        }
        Ok(Self {
            to_submit,
            minimum_complete,
            flags,
            argument: EnterArgument::Extended(argument),
        })
    }

    /// Maximum SQ entries requested by this invocation.
    pub const fn to_submit(self) -> u32 {
        self.to_submit
    }

    /// Completion count requested by `GETEVENTS` waiting.
    pub const fn minimum_complete(self) -> u32 {
        self.minimum_complete
    }

    /// Strictly decoded enter flags.
    pub const fn flags(self) -> EnterFlags {
        self.flags
    }

    /// Optional exact-size legacy mask request.
    pub const fn signal_mask(self) -> LegacySignalMask {
        match self.argument {
            EnterArgument::Legacy(mask) => mask,
            EnterArgument::Extended(_) => LegacySignalMask::None,
        }
    }

    /// The copied argument interpretation for this invocation.
    pub const fn argument(self) -> EnterArgument {
        self.argument
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_getevents_argument_has_the_v618_layout() {
        let argument = IoUringGeteventsArg::from_ne_bytes([
            1, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 9, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let request = EnterRequest::from_extended(
            3,
            1,
            EnterFlags::GETEVENTS.bits() | EnterFlags::EXT_ARG.bits(),
            argument,
        )
        .unwrap();
        assert_eq!(request.argument(), EnterArgument::Extended(argument));
        assert_eq!(argument.sigmask_address(), 1);
        assert_eq!(argument.sigmask_size(), 8);
        assert_eq!(argument.min_wait_usec(), 9);
        assert_eq!(argument.timespec_address(), 2);
    }

    #[test]
    fn only_supported_enter_flags_are_accepted() {
        let request = EnterRequest::from_raw(3, 1, EnterFlags::GETEVENTS.bits(), 0, 0, 8).unwrap();
        assert_eq!(request.to_submit(), 3);
        assert_eq!(request.minimum_complete(), 1);
        assert!(request.flags().contains(EnterFlags::GETEVENTS));
        assert_eq!(
            EnterRequest::from_raw(0, 0, 1 << 3, 0, 0, 8),
            Err(IoUringError::InvalidSignalMaskArgument)
        );
    }

    #[test]
    fn legacy_signal_mask_requires_the_exact_consumer_size() {
        assert_eq!(
            EnterRequest::from_raw(0, 0, 0, 0x1000, 8, 8)
                .unwrap()
                .signal_mask(),
            LegacySignalMask::Address(0x1000)
        );
        assert_eq!(
            EnterRequest::from_raw(0, 0, 0, 0x1000, 16, 8),
            Err(IoUringError::InvalidSignalMaskArgument)
        );
        // A NULL legacy argp bypasses sigset-size validation in Linux.
        assert_eq!(
            EnterRequest::from_raw(0, 0, 0, 0, 8, 16)
                .unwrap()
                .signal_mask(),
            LegacySignalMask::None
        );
        assert_eq!(
            EnterRequest::from_raw(0, 0, 0, 0, 8, 8)
                .unwrap()
                .signal_mask(),
            LegacySignalMask::None
        );
    }
}
