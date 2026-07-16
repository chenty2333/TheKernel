#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]

extern crate alloc;

use core::num::NonZeroU64;

mod area;
mod backend;
mod set;

#[cfg(test)]
mod tests;

pub use self::{area::MemoryArea, backend::MappingBackend, set::MemorySet};

/// Caller-allocated opaque identity shared by fragments of one logical mapping.
///
/// `memory_set` never interprets or allocates this value. Splitting an area
/// preserves it, and adjacent areas may merge only when their lineages match.
/// A personality or VM adapter can therefore keep richer identity/generation
/// state outside this generic range container without teaching it Linux rules.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MappingLineage(NonZeroU64);

impl MappingLineage {
    /// Legacy lineage used by callers that do not track logical mappings.
    ///
    /// Sharing this value preserves `memory_set`'s historical behavior: areas
    /// with compatible flags and backends may merge. Identity-aware consumers
    /// should allocate their own value and call
    /// [`MemoryArea::new_with_lineage`].
    pub const UNTRACKED: Self = Self(NonZeroU64::MIN);

    /// Creates a tracked lineage from a caller-owned value.
    ///
    /// Zero and the compatibility-only [`Self::UNTRACKED`] value are reserved.
    pub const fn new(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) if raw.get() == Self::UNTRACKED.get() => None,
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }

    /// Returns the opaque numeric value for adapter-owned sidecar lookup.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Error type for memory mapping operations.
#[derive(Debug, Eq, PartialEq)]
pub enum MappingError {
    /// Invalid parameter (e.g., `addr`, `size`, `flags`, etc.)
    InvalidParam,
    /// Fallible temporary staging, checked fragment arithmetic, or a
    /// caller-supplied live-area quota rejected the operation.
    ///
    /// This does not claim recoverable allocation failure for `BTreeMap` node
    /// insertion; `alloc` currently exposes no fallible insertion API for it.
    NoMemory,
    /// The given range overlaps with an existing mapping.
    AlreadyExists,
    /// The backend page table is in a bad state.
    BadState,
}

#[cfg(feature = "axerrno")]
impl From<MappingError> for axerrno::AxError {
    fn from(err: MappingError) -> Self {
        match err {
            MappingError::InvalidParam => axerrno::AxError::InvalidInput,
            MappingError::NoMemory => axerrno::AxError::NoMemory,
            MappingError::AlreadyExists => axerrno::AxError::AlreadyExists,
            MappingError::BadState => axerrno::AxError::BadState,
        }
    }
}

/// A [`Result`] type with [`MappingError`] as the error type.
pub type MappingResult<T = ()> = Result<T, MappingError>;
