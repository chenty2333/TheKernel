//! Private shmem mount for anonymous memfds. No caller pathname is involved.
use alloc::{sync::Arc, vec::Vec};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{
    AnonymousOptions, DirEntry, FsName, FsPathBuf, Location, Mountpoint, NodePermission, NodeType,
    Reference,
};
use spin::Once;
use tk_linux_mm::{F_SEAL_EXEC, F_SEAL_SEAL, MemfdPlan, inode_mode};

use crate::pseudofs::tmp::MemoryFs;

static SHMEM: Once<Arc<Mountpoint>> = Once::new();
struct MemfdLabel(Vec<u8>);

fn provider() -> AxResult<&'static Arc<Mountpoint>> {
    SHMEM
        .try_call_once(|| {
            let fs = MemoryFs::new()?;
            Mountpoint::new_root_at_with_extensions(&fs, fs.root_dir(), axfs_ng_vfs::TypeMap::new())
        })
}

pub(crate) fn is_internal_mount(mount: &Arc<Mountpoint>) -> bool {
    SHMEM
        .get()
        .is_some_and(|internal| Arc::ptr_eq(internal, mount))
}

pub(crate) fn label(loc: &Location) -> Option<AxResult<FsPathBuf>> {
    let label = loc.user_data().get::<MemfdLabel>()?;
    Some((|| {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(label.0.len())
            .map_err(|_| AxError::NoMemory)?;
        bytes.extend_from_slice(&label.0);
        Ok(FsPathBuf::from_vec(bytes))
    })())
}

pub(crate) fn create(name: &[u8], plan: MemfdPlan, uid: u32, gid: u32) -> AxResult<Location> {
    let mut label = Vec::new();
    label
        .try_reserve_exact(b"/memfd:".len() + name.len())
        .map_err(|_| AxError::NoMemory)?;
    label.extend_from_slice(b"/memfd:");
    label.extend_from_slice(name);
    let location = provider()?
        .root_location()
        .create_anonymous(&AnonymousOptions {
            node_type: NodeType::RegularFile,
            permission: NodePermission::from_bits_truncate(inode_mode(plan)),
            user: Some((uid, gid)),
            linkable: false,
        })?;
    // A private alias supplies a stable display path to VFS policy without
    // publishing a directory entry or adding a hard link to the inode.
    let entry = DirEntry::try_new_file(
        axfs_ng_vfs::FileNode::new(location.entry().as_file()?.inner().clone()),
        NodeType::RegularFile,
        Reference::try_new(
            Some(provider()?.root_location().entry().clone()),
            FsName::new(b"memfd"),
        )?,
    )?;
    let location = Location::new(provider()?.clone(), entry);
    location
        .user_data()
        .try_get_or_insert_with(|| MemfdLabel(label))?;
    super::memfd::install_memfd_state(
        &location,
        if plan.may_seal {
            if plan.noexec_seal { F_SEAL_EXEC } else { 0 }
        } else {
            F_SEAL_SEAL
        },
    )?;
    Ok(location)
}

#[cfg(test)]
mod tests {
    use linux_raw_sys::general::MFD_ALLOW_SEALING;
    use tk_linux_mm::sanitize_flags;

    use super::*;

    #[test]
    fn anonymous_memfd_has_no_namespace_link_and_preserves_opaque_name() {
        let plan = sanitize_flags(MFD_ALLOW_SEALING, 0).unwrap();
        let loc = create(b"crun:/proc/self/exe", plan, 123, 456).unwrap();
        let stat = loc.metadata().unwrap();
        assert_eq!(stat.nlink, 0);
        assert_eq!((stat.uid, stat.gid), (123, 456));
        assert_eq!(stat.mode.bits(), 0o777);
        assert_eq!(
            label(&loc).unwrap().unwrap().as_bytes(),
            b"/memfd:crun:/proc/self/exe"
        );
        assert!(is_internal_mount(loc.mountpoint()));
        assert_eq!(super::super::memfd::get_seals(&loc), Ok(0));
        let other_fs = MemoryFs::new().unwrap();
        assert!(!is_internal_mount(&Mountpoint::new_root(&other_fs)));
    }
}
