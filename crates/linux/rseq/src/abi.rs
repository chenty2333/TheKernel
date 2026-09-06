use core::mem::{align_of, offset_of, size_of};

use crate::RseqError;

/// Original Linux v6.6 `struct rseq` size, including trailing ABI padding.
pub const RSEQ_AREA_SIZE: usize = 32;
/// Original Linux v6.6 `struct rseq` alignment.
pub const RSEQ_AREA_ALIGN: usize = 32;
/// Linux 7.2.3 feature extent (offsetof(struct rseq, end)).
pub const RSEQ_ABI_SIZE: usize = 33;
/// Linux 7.2.3 alignment for extended registrations.
pub const RSEQ_ABI_ALIGN: usize = 64;
/// Linux v6.6 `struct rseq_cs` size.
pub const RSEQ_CS_SIZE: usize = 32;
/// Linux v6.6 `struct rseq_cs` alignment.
pub const RSEQ_CS_ALIGN: usize = 32;
/// Number of bytes occupied by the signature word immediately before abort.
pub const RSEQ_CS_SIGNATURE_BYTES: u64 = size_of::<u32>() as u64;
/// The TheKernel v6.6 profile accepts no `rseq_cs.flags` bits in the restart
/// path.  This deliberately keeps the component's restart contract at the
/// zero-flags subset.
pub const RSEQ_CS_SUPPORTED_FLAGS: u32 = 0;
/// The TheKernel v6.6 profile accepts no `rseq.flags` bits in the restart
/// path.  This deliberately keeps the component's restart contract at the
/// zero-flags subset.
pub const RSEQ_AREA_SUPPORTED_FLAGS: u32 = 0;
/// `rseq(2)` unregister operation flag.
pub const RSEQ_FLAG_UNREGISTER: u32 = 1;
/// Requests slice extension by default if the kernel implements it.
pub const RSEQ_FLAG_SLICE_EXT_DEFAULT_ON: u32 = 2;
/// Linux sentinel written while an area is not initialized.
pub const RSEQ_CPU_ID_UNINITIALIZED: u32 = u32::MAX;
/// Linux sentinel written when registration initialization failed.
pub const RSEQ_CPU_ID_REGISTRATION_FAILED: u32 = u32::MAX - 1;

/// Exclusive upper bound of the user address space used to validate a copied
/// descriptor.  Keeping the bound in a dedicated type prevents a raw,
/// unbounded descriptor validation path from reaching the restart gate.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UserAddressLimit(u64);

impl UserAddressLimit {
    /// Creates an adapter-owned exclusive user address limit.
    ///
    /// The adapter obtains this value from its address-space policy.  This
    /// crate does not guess an architecture-specific `TASK_SIZE`.
    pub const fn new(exclusive: u64) -> Self {
        Self(exclusive)
    }

    /// Raw exclusive upper bound.
    pub const fn exclusive(self) -> u64 {
        self.0
    }

    /// Whether one address is a valid user address under this proof.
    pub const fn contains(self, address: u64) -> bool {
        address < self.0
    }

    /// Whether a byte range is entirely below this exclusive bound.
    pub const fn contains_range(self, start: u64, length: u64) -> bool {
        match start.checked_add(length) {
            Some(end) => end <= self.0,
            None => false,
        }
    }
}

/// Linux v6.6 restartable-sequence area.
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RseqArea {
    /// CPU observed before entering the userspace sequence.
    pub cpu_id_start: u32,
    /// CPU observed by the userspace sequence before commit.
    pub cpu_id: u32,
    /// User address of the active [`RseqCriticalSection`], or zero.
    pub rseq_cs: u64,
    /// Area flags; this profile requires the zero mask.
    pub flags: u32,
    /// NUMA node ID maintained by the adapter/kernel.
    pub node_id: u32,
    /// Memory-map concurrency ID maintained by the adapter/kernel.
    pub mm_cid: u32,
}

impl RseqArea {
    /// Returns a registration-initialized area value.
    pub const fn initial() -> Self {
        Self {
            cpu_id_start: RSEQ_CPU_ID_UNINITIALIZED,
            cpu_id: RSEQ_CPU_ID_UNINITIALIZED,
            rseq_cs: 0,
            flags: 0,
            node_id: 0,
            mm_cid: 0,
        }
    }

    /// Alias for [`Self::initial`].
    pub const fn new() -> Self {
        Self::initial()
    }

    /// Whether a userspace critical-section descriptor is active.
    pub const fn has_active_critical_section(self) -> bool {
        self.rseq_cs != 0
    }

    /// Clears the modeled active descriptor value without touching user
    /// memory.
    pub const fn cleared(mut self) -> Self {
        self.rseq_cs = 0;
        self
    }

    /// Clears the modeled active descriptor value in a caller-owned copy.
    pub fn clear_critical_section(&mut self) {
        self.rseq_cs = 0;
    }

    /// Validates area flags for the in-critical-section restart path.
    ///
    /// The restart gate deliberately calls this only after it has established
    /// that the saved IP is inside the descriptor interval.  An out-of-range
    /// IP is always a clear-only case, even when user flags are non-zero.
    pub const fn validate_restart_flags(self) -> Result<(), RseqError> {
        if self.flags & !RSEQ_AREA_SUPPORTED_FLAGS != 0 {
            Err(RseqError::InvalidAreaFlags)
        } else {
            Ok(())
        }
    }

    /// Classifies the two CPU fields, including Linux's reserved sentinels.
    pub const fn cpu_check(self) -> CpuCheck {
        if self.cpu_id_start == RSEQ_CPU_ID_REGISTRATION_FAILED
            || self.cpu_id == RSEQ_CPU_ID_REGISTRATION_FAILED
        {
            return CpuCheck::RegistrationFailed;
        }
        if self.cpu_id_start == RSEQ_CPU_ID_UNINITIALIZED
            || self.cpu_id == RSEQ_CPU_ID_UNINITIALIZED
        {
            return CpuCheck::Uninitialized;
        }
        if self.cpu_id_start == self.cpu_id {
            CpuCheck::Match
        } else {
            CpuCheck::Mismatch
        }
    }

    /// Checks the area against one caller-supplied CPU number.
    pub const fn check_cpu(self, expected: u32) -> Result<(), RseqError> {
        if expected == RSEQ_CPU_ID_UNINITIALIZED || expected == RSEQ_CPU_ID_REGISTRATION_FAILED {
            return Err(RseqError::InvalidCpuId);
        }
        match self.cpu_check() {
            CpuCheck::Match if self.cpu_id == expected => Ok(()),
            CpuCheck::Uninitialized => Err(RseqError::CpuIdUninitialized),
            CpuCheck::RegistrationFailed => Err(RseqError::CpuIdRegistrationFailed),
            CpuCheck::Match | CpuCheck::Mismatch => Err(RseqError::CpuIdMismatch),
        }
    }
}

/// Result of comparing the saved and current CPU values in an area.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuCheck {
    /// Both values are ordinary CPU IDs and equal.
    Match,
    /// Both values are ordinary CPU IDs but differ.
    Mismatch,
    /// At least one value is `RSEQ_CPU_ID_UNINITIALIZED`.
    Uninitialized,
    /// At least one value is `RSEQ_CPU_ID_REGISTRATION_FAILED`.
    RegistrationFailed,
}

/// Classification of one raw Linux CPU field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuIdState {
    /// A regular CPU number.
    Valid(u32),
    /// `RSEQ_CPU_ID_UNINITIALIZED`.
    Uninitialized,
    /// `RSEQ_CPU_ID_REGISTRATION_FAILED`.
    RegistrationFailed,
}

impl CpuIdState {
    /// Decodes a raw Linux CPU ID without treating sentinels as ordinary IDs.
    pub const fn from_raw(raw: u32) -> Self {
        if raw == RSEQ_CPU_ID_UNINITIALIZED {
            Self::Uninitialized
        } else if raw == RSEQ_CPU_ID_REGISTRATION_FAILED {
            Self::RegistrationFailed
        } else {
            Self::Valid(raw)
        }
    }

    /// Returns the raw Linux field value.
    pub const fn raw(self) -> u32 {
        match self {
            Self::Valid(raw) => raw,
            Self::Uninitialized => RSEQ_CPU_ID_UNINITIALIZED,
            Self::RegistrationFailed => RSEQ_CPU_ID_REGISTRATION_FAILED,
        }
    }
}

/// Linux v6.6 restartable-sequence descriptor.
#[repr(C, align(32))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RseqCriticalSection {
    /// Descriptor format version; v6.6 requires zero.
    pub version: u32,
    /// Descriptor flags; this profile validates the zero mask only for an
    /// in-critical-section restart.
    pub flags: u32,
    /// First instruction in the critical section.
    pub start_ip: u64,
    /// Byte offset from `start_ip` to the post-commit instruction.
    pub post_commit_offset: u64,
    /// Instruction address to which an interrupted sequence resumes.
    pub abort_ip: u64,
}

impl RseqCriticalSection {
    /// Builds a version-zero, flag-free descriptor.
    pub const fn new(start_ip: u64, post_commit_offset: u64, abort_ip: u64) -> Self {
        Self {
            version: 0,
            flags: 0,
            start_ip,
            post_commit_offset,
            abort_ip,
        }
    }

    /// Builds a descriptor from every ABI field.
    pub const fn from_raw(
        version: u32,
        flags: u32,
        start_ip: u64,
        post_commit_offset: u64,
        abort_ip: u64,
    ) -> Self {
        Self {
            version,
            flags,
            start_ip,
            post_commit_offset,
            abort_ip,
        }
    }

    /// Checked post-commit instruction address.
    pub const fn post_commit_ip(self) -> Result<u64, RseqError> {
        match self.start_ip.checked_add(self.post_commit_offset) {
            Some(ip) => Ok(ip),
            None => Err(RseqError::AddressOverflow),
        }
    }

    /// Checked address immediately before `abort_ip` where Linux reads the
    /// registration signature word.
    pub const fn signature_address(self) -> Result<u64, RseqError> {
        match self.abort_ip.checked_sub(RSEQ_CS_SIGNATURE_BYTES) {
            Some(address) => Ok(address),
            None => Err(RseqError::SignatureAddressUnderflow),
        }
    }

    /// Tests whether an instruction pointer is in the half-open critical
    /// section interval `[start_ip, post_commit_ip)`.
    pub fn contains(self, instruction_pointer: u64) -> Result<bool, RseqError> {
        let end = self.post_commit_ip()?;
        Ok(instruction_pointer >= self.start_ip && instruction_pointer < end)
    }

    /// Validates descriptor arithmetic and all user code addresses against an
    /// exclusive user limit.  Flags are intentionally checked by the restart
    /// gate only after IP interval classification.
    pub(crate) fn validate_for_user(self, user_limit: UserAddressLimit) -> Result<(), RseqError> {
        if self.version != 0 {
            return Err(RseqError::InvalidVersion);
        }
        let end = self.post_commit_ip()?;
        if !user_limit.contains(self.start_ip)
            || !user_limit.contains(end)
            || !user_limit.contains(self.abort_ip)
        {
            return Err(RseqError::AddressOutOfRange);
        }
        if self.contains(self.abort_ip)? {
            return Err(RseqError::AbortInCriticalSection);
        }
        Ok(())
    }

    /// Validates descriptor flags for the in-critical-section restart path.
    pub const fn validate_restart_flags(self) -> Result<(), RseqError> {
        if self.flags & !RSEQ_CS_SUPPORTED_FLAGS != 0 {
            Err(RseqError::InvalidFlags)
        } else {
            Ok(())
        }
    }

    /// Verifies that a caller-provided address names the signature word Linux
    /// reads immediately before the abort target.
    pub fn validate_signature_address(self, signature_address: u64) -> Result<(), RseqError> {
        if self.signature_address()? == signature_address {
            Ok(())
        } else {
            Err(RseqError::SignatureAddressMismatch)
        }
    }
}

const _: () = {
    assert!(size_of::<RseqArea>() == RSEQ_AREA_SIZE);
    assert!(align_of::<RseqArea>() == RSEQ_AREA_ALIGN);
    assert!(offset_of!(RseqArea, cpu_id_start) == 0);
    assert!(offset_of!(RseqArea, cpu_id) == 4);
    assert!(offset_of!(RseqArea, rseq_cs) == 8);
    assert!(offset_of!(RseqArea, flags) == 16);
    assert!(offset_of!(RseqArea, node_id) == 20);
    assert!(offset_of!(RseqArea, mm_cid) == 24);
    assert!(size_of::<RseqCriticalSection>() == RSEQ_CS_SIZE);
    assert!(align_of::<RseqCriticalSection>() == RSEQ_CS_ALIGN);
    assert!(offset_of!(RseqCriticalSection, version) == 0);
    assert!(offset_of!(RseqCriticalSection, flags) == 4);
    assert!(offset_of!(RseqCriticalSection, start_ip) == 8);
    assert!(offset_of!(RseqCriticalSection, post_commit_offset) == 16);
    assert!(offset_of!(RseqCriticalSection, abort_ip) == 24);
};
