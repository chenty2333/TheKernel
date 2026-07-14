use alloc::vec::Vec;

use axerrno::LinuxError;

use crate::{VfsError, VfsResult};

/// Mutation semantics for one extended attribute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XattrSetMode {
    /// Insert a missing attribute or replace an existing value.
    Upsert,
    /// Insert only when the attribute is absent.
    Create,
    /// Replace only when the attribute is present.
    Replace,
}

/// Stable per-inode extended-attribute storage.
///
/// Every method returns a result produced while the provider still owns its
/// serialization boundary. In particular, implementations must keep a size
/// probe and snapshot copy together, and must serialize a create/replace
/// existence decision with the corresponding mutation.
///
/// Names are opaque bytes. Namespace interpretation and ABI-specific length
/// limits belong above this generic storage contract.
pub trait XattrProvider: Send + Sync {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>>;

    /// Returns complete names separated and terminated by NUL bytes.
    fn list_xattrs(&self) -> VfsResult<Vec<u8>>;

    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()>;

    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()>;
}

pub(crate) fn unsupported_xattr() -> VfsError {
    LinuxError::EOPNOTSUPP.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_provider_is_honestly_unsupported_for_general_xattrs() {
        assert_eq!(
            LinuxError::from(unsupported_xattr()),
            LinuxError::EOPNOTSUPP
        );
    }
}
