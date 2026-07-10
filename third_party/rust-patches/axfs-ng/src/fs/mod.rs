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

#[cfg(feature = "ext4")]
mod ext4;
#[cfg(feature = "fat")]
mod fat;

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

    Err(axfs_ng_vfs::VfsError::InvalidInput)
}
