use crate::{RseqCriticalSection, RseqError, UserAddressLimit};

/// A descriptor address paired with its copied Linux `rseq_cs` value and an
/// exclusive user-limit proof.  The proof covers the descriptor pointer and
/// code addresses; the adapter owns the copied 32-byte span and its `EFAULT`
/// result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RseqDescriptor {
    address: u64,
    critical_section: RseqCriticalSection,
    user_limit: UserAddressLimit,
}

impl RseqDescriptor {
    /// Builds a descriptor object only after proving that the descriptor
    /// object and every code address are below the exclusive user limit.
    ///
    /// There is intentionally no unbounded `validate()` or raw-limit
    /// constructor.  The restart gate accepts this proof-bearing object only.
    pub fn new(
        address: u64,
        critical_section: RseqCriticalSection,
        user_limit: UserAddressLimit,
    ) -> Result<Self, RseqError> {
        if address == 0 {
            return Err(RseqError::InvalidDescriptorAddress);
        }
        // Linux first validates the descriptor pointer itself.  The adapter
        // must then perform the 32-byte usercopy and map a fault (including a
        // copy which crosses the user limit) to EFAULT.  Keeping the span
        // check out of this pure proof prevents that adapter-side fault from
        // being misclassified as restart EINVAL.
        if !user_limit.contains(address) {
            return Err(RseqError::AddressOutOfRange);
        }
        critical_section.validate_for_user(user_limit)?;
        Ok(Self {
            address,
            critical_section,
            user_limit,
        })
    }

    /// Alias documenting that the copied descriptor came from user memory.
    pub fn from_user(
        address: u64,
        critical_section: RseqCriticalSection,
        user_limit: UserAddressLimit,
    ) -> Result<Self, RseqError> {
        Self::new(address, critical_section, user_limit)
    }

    /// Descriptor address as stored in `RseqArea::rseq_cs`.
    pub const fn address(self) -> u64 {
        self.address
    }

    /// Copied ABI descriptor value.
    pub const fn critical_section(self) -> RseqCriticalSection {
        self.critical_section
    }

    /// Exclusive user-limit proof carried by this object.
    pub const fn user_limit(self) -> UserAddressLimit {
        self.user_limit
    }

    /// Address of the signature word Linux checks before the abort target.
    pub const fn signature_address(self) -> Result<u64, RseqError> {
        self.critical_section.signature_address()
    }

    /// Revalidates a caller-provided signature-word address.
    pub fn validate_signature_address(self, signature_address: u64) -> Result<(), RseqError> {
        self.critical_section
            .validate_signature_address(signature_address)
    }

    /// Validates area/descriptor flags for an in-critical-section restart.
    /// The restart gate calls this only after interval classification.
    pub const fn validate_restart_flags(self, area_flags: u32) -> Result<(), RseqError> {
        if area_flags != 0 {
            return Err(RseqError::InvalidAreaFlags);
        }
        self.critical_section.validate_restart_flags()
    }

    /// Returns whether an instruction pointer is inside this descriptor's
    /// half-open critical-section interval.
    pub fn contains(self, instruction_pointer: u64) -> Result<bool, RseqError> {
        self.critical_section.contains(instruction_pointer)
    }
}

/// Descriptor proof after user-limit and structural validation.  It is an
/// alias rather than a second constructor so no raw validation path exists.
pub type ValidatedRseqDescriptor = RseqDescriptor;

/// Structural descriptor proof retained for source compatibility with
/// adapters that only need the copied value.  Construction remains private to
/// this crate; restart decisions require [`RseqDescriptor`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ValidatedRseqCriticalSection(pub(crate) RseqCriticalSection);

impl ValidatedRseqCriticalSection {
    /// Returns the validated ABI descriptor.
    pub const fn get(self) -> RseqCriticalSection {
        self.0
    }

    /// Alias for [`Self::get`].
    pub const fn descriptor(self) -> RseqCriticalSection {
        self.0
    }
}

/// Validates a copied descriptor and its expected signature-word address
/// under an explicit exclusive user-limit proof.
pub fn validate_descriptor(
    address: u64,
    critical_section: RseqCriticalSection,
    signature_address: u64,
    user_limit: UserAddressLimit,
) -> Result<ValidatedRseqDescriptor, RseqError> {
    let descriptor = RseqDescriptor::new(address, critical_section, user_limit)?;
    descriptor.validate_signature_address(signature_address)?;
    Ok(descriptor)
}
