use axfs_ng_vfs::{Filesystem, NodePermission, VfsResult};

use crate::MountedBlockDevice;

#[derive(Clone, Copy, Debug)]
pub struct FatMountOptions {
    pub uid: u32,
    pub gid: u32,
    pub file_mode: NodePermission,
    pub dir_mode: NodePermission,
}

impl Default for FatMountOptions {
    fn default() -> Self {
        Self {
            uid: 0,
            gid: 0,
            file_mode: NodePermission::from_bits_truncate(0o777),
            dir_mode: NodePermission::from_bits_truncate(0o777),
        }
    }
}

#[cfg(feature = "btrfs")]
pub mod btrfs;
#[cfg(feature = "ext4")]
pub(crate) mod ext4;
#[cfg(feature = "btrfs")]
pub use btrfs::BtrfsFilesystem;
#[cfg(feature = "fat")]
mod fat;
/// Clean-room NFSv4.1 client protocol/session core. It is not registered by
/// `new_named`, because NFS mounting requires a live network transport.
#[cfg(feature = "nfs41")]
pub mod nfs;
/// Overlayfs mount-option and topology admission.
///
/// This deliberately stays outside `new_named`: a filesystem must not be
/// advertised to the mount registry until the node adapter can preserve every
/// overlay operation at its publication boundary.
#[cfg(feature = "overlay")]
pub mod overlay;
#[cfg(feature = "xfs")]
pub mod xfs;
#[cfg(feature = "xfs")]
mod xfs_vfs;
#[cfg(feature = "xfs")]
pub use xfs::*;
#[cfg(feature = "xfs")]
pub use xfs_vfs::{XfsFilesystem, XfsMountMembers};

cfg_if::cfg_if! {
    if #[cfg(feature = "ext4")] {
        type DefaultFilesystem = ext4::Ext4Filesystem;
    } else if #[cfg(feature = "fat")] {
        type DefaultFilesystem = fat::FatFilesystem;
    } else {
        struct DefaultFilesystem;
        impl DefaultFilesystem {
            pub fn new(_dev: MountedBlockDevice) -> VfsResult<Filesystem> {
                panic!("No filesystem feature enabled");
            }
        }
    }
}

pub fn new_default(dev: MountedBlockDevice) -> VfsResult<Filesystem> {
    DefaultFilesystem::new(dev)
}

pub fn new_named(
    fs_type: &str,
    dev: MountedBlockDevice,
    fat_options: Option<FatMountOptions>,
) -> VfsResult<Filesystem> {
    #[cfg(feature = "ext4")]
    if fs_type == "ext4" {
        return ext4::Ext4Filesystem::new(dev);
    }

    #[cfg(feature = "fat")]
    if matches!(fs_type, "vfat" | "fat" | "msdos") {
        return fat::FatFilesystem::new_with_options(dev, fat_options.unwrap_or_default());
    }

    #[cfg(feature = "btrfs")]
    if fs_type == "btrfs" {
        return btrfs::BtrfsFilesystem::new(dev);
    }

    #[cfg(feature = "xfs")]
    if fs_type == "xfs" {
        return XfsFilesystem::new(dev);
    }

    Err(axfs_ng_vfs::VfsError::NoSuchDevice)
}

/// Installs the task-context worker wakeup used by deferred filesystem
/// teardown.  Btrfs uses the same worker for last-close orphan retirement;
/// both queues are drained by the common scheduler hook.
pub fn set_deferred_filesystem_finalizer_waker(waker: fn()) -> bool {
    let mut installed = true;
    #[cfg(feature = "ext4")]
    {
        installed &= ext4::set_deferred_finalizer_waker(waker);
    }
    #[cfg(feature = "btrfs")]
    {
        installed &= btrfs::set_deferred_orphan_finalizer_waker(waker);
    }
    #[cfg(not(any(feature = "ext4", feature = "btrfs")))]
    let _ = waker;
    installed
}

pub fn has_deferred_filesystem_finalizer_work() -> bool {
    let mut pending = false;
    #[cfg(feature = "ext4")]
    {
        pending |= ext4::has_deferred_finalizer_work();
    }
    #[cfg(feature = "btrfs")]
    {
        pending |= btrfs::has_deferred_orphan_finalizer_work();
    }
    pending
}

pub fn drain_deferred_filesystem_finalizers(between: impl FnMut()) -> usize {
    let mut between = between;
    let mut drained: usize = 0;
    #[cfg(feature = "ext4")]
    {
        drained = drained.saturating_add(ext4::drain_deferred_finalizers(&mut between));
    }
    #[cfg(feature = "btrfs")]
    {
        drained = drained.saturating_add(btrfs::drain_deferred_orphan_finalizers(&mut between));
    }
    drained
}
