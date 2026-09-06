use core::num::NonZeroU64;

use crate::MmError;

macro_rules! nonzero_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Builds an identity, rejecting the reserved zero value.
            pub const fn new(raw: u64) -> Result<Self, MmError> {
                match NonZeroU64::new(raw) {
                    Some(raw) => Ok(Self(raw)),
                    None => Err(MmError::InvalidIdentity),
                }
            }

            /// Returns the consumer-visible integer representation.
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

nonzero_id!(
    AddressSpaceId,
    "Stable identity of one consumer-owned address space."
);
nonzero_id!(MappingId, "Stable identity of one logical mapping.");
nonzero_id!(PinOwner, "Stable accounting owner for pin resources.");

/// Nonzero generation of one logical mapping.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MappingGeneration(NonZeroU64);

impl MappingGeneration {
    /// Builds a generation, rejecting the reserved zero value.
    pub const fn new(raw: u64) -> Result<Self, MmError> {
        match NonZeroU64::new(raw) {
            Some(raw) => Ok(Self(raw)),
            None => Err(MmError::InvalidIdentity),
        }
    }

    /// Returns the integer generation.
    pub const fn get(self) -> u64 {
        self.0.get()
    }

    /// Returns the next generation without wrapping into ABA reuse.
    pub const fn next(self) -> Result<Self, MmError> {
        match self.get().checked_add(1) {
            Some(raw) => Self::new(raw),
            None => Err(MmError::IdExhausted),
        }
    }
}
