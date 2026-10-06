//! Live ownership of world-searchable proc task directories.
use axfs_ng_vfs::{Metadata, NodePermission};
use tk_linux_cred::CredentialIds;

/// Proc task directories use effective IDs even after a nondumpable exec or
/// credential transition. Other proc nodes retain their existing access rules.
pub(super) fn task_directory_metadata(
    mut metadata: Metadata,
    subject: Option<CredentialIds>,
) -> Metadata {
    let ids = subject.unwrap_or_else(CredentialIds::initial_root);
    metadata.mode = NodePermission::from_bits_truncate(0o555);
    metadata.uid = ids.euid.into_raw();
    metadata.gid = ids.egid.into_raw();
    metadata
}

#[cfg(test)]
mod tests {
    use alloc::{string::String, sync::Arc};

    use axfs_ng_vfs::{FilesystemOps, FsName, VfsError, VfsResult};
    use axsync::Mutex;
    use tk_linux_cred::{Kgid, Kuid};

    use super::*;
    use crate::pseudofs::{
        ChildNames, NodeOpsMux, SimpleDir, SimpleDirOps, SimpleFs, try_boxed_names,
    };

    struct TaskDirectory(Mutex<Option<CredentialIds>>);

    impl SimpleDirOps for TaskDirectory {
        fn directory_metadata(&self, metadata: Metadata) -> Metadata {
            task_directory_metadata(metadata, *self.0.lock())
        }
        fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
            try_boxed_names(core::iter::empty())
        }
        fn lookup_child(&self, _: &FsName) -> VfsResult<NodeOpsMux> {
            Err(VfsError::NotFound)
        }
    }

    #[test]
    fn retained_directory_tracks_effective_not_real_or_filesystem_ids() {
        let ops = Arc::new(TaskDirectory(Mutex::new(Some(
            CredentialIds::initial_root(),
        ))));
        let fs = SimpleFs::new_with(String::from("proc-owner-test"), 0x9fa0, {
            let ops = ops.clone();
            move |fs| SimpleDir::new_maker(fs, ops)
        });
        let root = fs.root_dir();
        let before = root.metadata().unwrap();
        assert_eq!((before.uid, before.gid, before.mode.bits()), (0, 0, 0o555));
        let mut ids = CredentialIds::initial_root();
        ids.euid = Kuid::from_raw(1000).unwrap();
        ids.egid = Kgid::from_raw(1001).unwrap();
        *ops.0.lock() = Some(ids);
        let after = root.metadata().unwrap();
        assert_eq!(
            (after.uid, after.gid, after.mode.bits()),
            (1000, 1001, 0o555)
        );
        assert_eq!(after.inode, before.inode);
        assert_eq!(after.node_type, before.node_type);
        *ops.0.lock() = None;
        let gone = root.metadata().unwrap();
        assert_eq!((gone.uid, gone.gid), (0, 0));
        assert_eq!(gone.inode, before.inode);
    }
}
