//! Namespace-retained network controls needed by OCI runtimes.
use alloc::{borrow::Cow, format, sync::Arc};

use axfs_ng_vfs::{FsName, Metadata, NodePermission, VfsError, VfsResult};
use axtask::current;
use linux_raw_sys::general::CAP_NET_ADMIN;

use super::{ChildNames, NodeOpsMux, SimpleDirOps, SimpleFile, SimpleFileOps, SimpleFs, try_boxed_names};
use crate::task::{AsThread, NetworkNamespace, ns_capable};

pub(super) struct PingSysctlDirectory {
    fs: Arc<SimpleFs>,
}

impl PingSysctlDirectory {
    pub(super) fn new(fs: Arc<SimpleFs>) -> Self {
        Self { fs }
    }
}

impl SimpleDirOps for PingSysctlDirectory {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        try_boxed_names(core::iter::once(Cow::Borrowed(FsName::new(b"ping_group_range"))))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        if name.as_bytes() != b"ping_group_range" {
            return Err(VfsError::NotFound);
        }
        Ok(SimpleFile::new_regular(self.fs.clone(), PingGroupRange {
            namespace: current().as_thread().net_ns(),
        }).into())
    }

    fn is_cacheable(&self) -> bool {
        false
    }
}

struct PingGroupRange {
    namespace: Arc<NetworkNamespace>,
}

fn parse_range(data: &[u8]) -> VfsResult<[u32; 2]> {
    let text = core::str::from_utf8(data).map_err(|_| VfsError::InvalidInput)?;
    let mut values = text.split_ascii_whitespace();
    let mut range = [0; 2];
    for value in &mut range {
        *value = values.next().and_then(|s| s.parse().ok()).filter(|v| *v != u32::MAX)
            .ok_or(VfsError::InvalidInput)?;
    }
    if values.next().is_some() {
        return Err(VfsError::InvalidInput);
    }
    Ok(range)
}

impl SimpleFileOps for PingGroupRange {
    fn default_permission(&self) -> NodePermission {
        NodePermission::from_bits_truncate(0o644)
    }

    fn file_metadata(&self, mut metadata: Metadata) -> Metadata {
        let owner = self.namespace.owner_user_ns();
        metadata.uid = owner.root_kuid().map(|id| id.into_raw()).unwrap_or(0);
        metadata.gid = owner.root_kgid().map(|id| id.into_raw()).unwrap_or(0);
        metadata
    }

    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let task = current();
        let cred = task.as_thread().current_cred();
        let [low, high] = self.namespace.ping_group_range();
        let ns = cred.user_ns();
        let project = |id| crate::task::Kgid::from_raw(id)
            .map(|gid| ns.from_kgid_munged(gid)).unwrap_or(65534);
        Ok(Cow::Owned(format!("{}\t{}\n", project(low), project(high)).into_bytes()))
    }

    fn write_all(&self, data: &[u8]) -> VfsResult<()> {
        let task = current();
        let cred = task.as_thread().current_cred();
        if self.namespace.owner_user_ns().root_kuid() != Some(cred.fs_dac_credentials().uid())
            && !ns_capable(&cred, self.namespace.owner_user_ns(), CAP_NET_ADMIN)
        {
            return Err(VfsError::PermissionDenied);
        }
        let [low, high] = parse_range(data)?;
        let ns = cred.user_ns();
        let mapped_low = ns.make_kgid(low).ok_or(VfsError::InvalidInput)?.into_raw();
        let mapped_high = ns.make_kgid(high).ok_or(VfsError::InvalidInput)?.into_raw();
        self.namespace.set_ping_group_range(if high < low || mapped_high < mapped_low {
            [1, 0]
        } else {
            [mapped_low, mapped_high]
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_ranges_are_namespace_local_and_default_deny() {
        let _context = crate::test_support::scheduler_test_context();
        let owner = crate::task::UserNamespace::try_new_root().unwrap();
        let first = NetworkNamespace::try_new_loopback_only(owner.clone()).unwrap();
        let second = NetworkNamespace::try_new_loopback_only(owner).unwrap();
        assert_eq!(first.ping_group_range(), [1, 0]);
        first.set_ping_group_range([1000, 1005]);
        assert_eq!(first.ping_group_range(), [1000, 1005]);
        assert_eq!(second.ping_group_range(), [1, 0]);
    }

    #[test]
    fn ping_range_parser_rejects_invalid_groups_and_incomplete_values() {
        assert_eq!(parse_range(b"0\t65535\n").unwrap(), [0, 65535]);
        assert_eq!(parse_range(b"9 2").unwrap(), [9, 2]);
        for invalid in [b"0".as_slice(), b"-1 3", b"0 4294967295", b"0 2 3", b"x 2"] {
            assert!(parse_range(invalid).is_err());
        }
    }
}
