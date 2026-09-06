use core::fmt;

/// Errors produced by credential value and authorization operations.
///
/// A kernel adapter maps these policy-neutral values to its local errno type.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredError {
    /// An input value or structure is malformed.
    InvalidInput,
    /// The operation is structurally valid but is not authorized.
    NotPermitted,
    /// A required allocation could not be completed.
    NoMemory,
    /// A bounded credential or namespace resource is exhausted.
    Capacity,
}

impl fmt::Display for CredError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput => f.write_str("invalid credential input"),
            Self::NotPermitted => f.write_str("credential operation not permitted"),
            Self::NoMemory => f.write_str("credential allocation failed"),
            Self::Capacity => f.write_str("credential resource capacity exhausted"),
        }
    }
}
