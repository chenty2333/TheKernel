//! Basic virtual filesystem support

pub mod cgroup;
pub mod dev;
mod device;
mod dir;
mod file;
mod fs;
mod proc;
mod sys;
pub(crate) mod tmp;

use alloc::{string::ToString, sync::Arc};

use axerrno::LinuxResult;
use axfs::{FS_CONTEXT, FsContext};
use axfs_ng_vfs::{DirNodeOps, FileNodeOps, Filesystem, NodePermission, WeakDirEntry};
pub use tmp::MemoryFs;

pub(crate) use self::proc::{
    ProcDirProcess, ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
    namespace_target_from_proc_file, proc_namespace_location_from_object,
    process_data_from_proc_dir,
};
pub use self::{device::*, dir::*, file::*, fs::*};
use crate::mounts;

/// A callback that builds a `Arc<dyn DirNodeOps>` for a given
/// `WeakDirEntry`.
pub type DirMaker = Arc<dyn Fn(WeakDirEntry) -> Arc<dyn DirNodeOps> + Send + Sync>;

/// An enum containing either a directory ([`DirMaker`]) or a file (`Arc<dyn
/// FileNodeOps>`).
#[derive(Clone)]
pub enum NodeOpsMux {
    /// A directory node.
    Dir(DirMaker),
    /// A file node.
    File(Arc<dyn FileNodeOps>),
}

impl From<DirMaker> for NodeOpsMux {
    fn from(maker: DirMaker) -> Self {
        Self::Dir(maker)
    }
}

impl<T: FileNodeOps> From<Arc<T>> for NodeOpsMux {
    fn from(ops: Arc<T>) -> Self {
        Self::File(ops)
    }
}

const DIR_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o755);
const VAR_TMP_CAPACITY_BYTES: u64 = 512 * 1024 * 1024;

fn mount_at(fs: &FsContext, path: &str, mount_fs: Filesystem) -> LinuxResult<()> {
    if fs.resolve(path).is_err() {
        fs.create_dir(path, DIR_PERMISSION)?;
    }
    let mountpoint = fs.resolve(path)?.mount(&mount_fs)?;
    if path != "/proc" {
        let fs_type = mount_fs.name().to_string();
        mounts::record(
            fs_type.clone(),
            path.to_string(),
            fs_type,
            mountpoint.device(),
            0,
        );
    }
    info!("Mounted {} at {}", mount_fs.name(), path);
    Ok(())
}

/// Mount all filesystems
pub fn mount_all() -> LinuxResult<()> {
    info!("Initialize pseudofs...");

    let fs = FS_CONTEXT.lock();
    mount_at(&fs, "/dev", dev::new_devfs())?;
    let tmp_permission = NodePermission::from_bits_truncate(0o1777);
    mount_at(
        &fs,
        "/dev/shm",
        tmp::MemoryFs::new_with_permission(tmp_permission),
    )?;
    mount_at(
        &fs,
        "/tmp",
        tmp::MemoryFs::new_with_permission(tmp_permission),
    )?;
    if fs.resolve("/var").is_err() {
        fs.create_dir("/var", DIR_PERMISSION)?;
    }
    mount_at(
        &fs,
        "/var/tmp",
        tmp::MemoryFs::new_with_permission_and_capacity(
            tmp_permission,
            Some(VAR_TMP_CAPACITY_BYTES),
        ),
    )?;
    mount_at(&fs, "/proc", proc::new_procfs())?;

    mount_at(&fs, "/sys", sys::new_sysfs())?;
    drop(fs);

    #[cfg(feature = "dev-log")]
    dev::bind_dev_log().expect("Failed to bind /dev/log");

    Ok(())
}
