//! Read-only capability ABI bounds, shared with the real admission type.
use alloc::{format, string::String, sync::Arc};

use axfs_ng_vfs::VfsResult;
use tk_linux_cred::CapabilityNumber;

use super::{SimpleFile, SimpleFs};

fn last_cap_text() -> String {
    format!("{}\n", CapabilityNumber::MAX)
}

pub(super) fn last_cap_file(fs: Arc<SimpleFs>) -> Arc<SimpleFile> {
    SimpleFile::new_regular(fs, || -> VfsResult<String> { Ok(last_cap_text()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_bound_matches_admission_and_linux_723() {
        assert_eq!(last_cap_text(), "40\n");
        assert!(CapabilityNumber::try_new(CapabilityNumber::MAX).is_some());
        assert!(CapabilityNumber::try_new(CapabilityNumber::MAX + 1).is_none());
    }
}
