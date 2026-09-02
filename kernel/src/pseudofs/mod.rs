//! Basic virtual filesystem support

pub mod cgroup;
pub mod dev;
mod device;
pub(crate) mod device_registry;
mod dir;
mod file;
mod fs;
mod graphics_metrics;
pub(crate) mod hugetlb;
#[cfg(feature = "test-io-control")]
mod io_test_control;
pub(crate) mod mqueue;
pub(crate) mod proc;
pub(crate) mod rpc_pipefs;
pub(crate) mod sys;
pub(crate) mod tmp;
pub(crate) mod trace;

use alloc::sync::Arc;

use axerrno::LinuxResult;
use axfs::FsContext;
use axfs_ng_vfs::{DirNodeOps, FileNodeOps, Filesystem, FsPath, NodePermission, WeakDirEntry};
use axnet::unix::UnixNamespace;
pub use tmp::MemoryFs;

pub(crate) use self::proc::{
    ProcDirProcess, ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
    bpf_network_namespace_from_fd, check_proc_pid_dir_search, namespace_target_from_proc_file,
    proc_namespace_location_from_object, process_data_from_proc_dir,
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

fn is_missing_path_error(error: axfs_ng_vfs::VfsError) -> bool {
    error.canonicalize() == axfs_ng_vfs::VfsError::NotFound
}

fn ensure_dir(fs: &FsContext, path: &FsPath) -> LinuxResult<()> {
    match fs.resolve(path) {
        Ok(_) => Ok(()),
        Err(error) if is_missing_path_error(error) => {
            fs.create_dir(path, DIR_PERMISSION)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn mount_at(fs: &FsContext, path: &FsPath, mount_fs: Filesystem) -> LinuxResult<()> {
    ensure_dir(fs, path)?;
    let target = fs.resolve(path)?;
    let mountpoint = mounts::new_detached_with_flags(
        &mount_fs,
        0,
        mounts::MountMetadata::try_from_parts(
            FsPath::new(mount_fs.name().as_bytes()),
            mount_fs.name(),
            FsPath::new(b"/"),
            "",
        )?,
    )?;
    mounts::attach_tree_and_record(&mountpoint, &target)?;
    info!("Mounted {} at {:?}", mount_fs.name(), path);
    Ok(())
}

/// Mount all filesystems
pub fn mount_all(
    fs: &FsContext,
    boot_security: &crate::file::permission::VfsSecurityContext,
    unix_namespace: Arc<UnixNamespace>,
) -> LinuxResult<()> {
    info!("Initialize pseudofs...");
    #[cfg(not(feature = "dev-log"))]
    let _ = (boot_security, unix_namespace);

    mount_at(fs, FsPath::new(b"/dev"), dev::new_devfs())?;
    let tmp_permission = NodePermission::from_bits_truncate(0o1777);
    mount_at(
        fs,
        FsPath::new(b"/dev/shm"),
        tmp::MemoryFs::new_with_permission(tmp_permission)?,
    )?;
    mount_at(
        fs,
        FsPath::new(b"/tmp"),
        tmp::MemoryFs::new_with_permission(tmp_permission)?,
    )?;
    ensure_dir(fs, FsPath::new(b"/var"))?;
    mount_at(
        fs,
        FsPath::new(b"/var/tmp"),
        tmp::MemoryFs::new_with_permission_and_capacity(
            tmp_permission,
            Some(VAR_TMP_CAPACITY_BYTES),
        )?,
    )?;
    mount_at(fs, FsPath::new(b"/proc"), proc::new_procfs())?;

    mount_at(fs, FsPath::new(b"/sys"), sys::new_sysfs())?;
    // fusectl is a real dynamic control filesystem.  Its non-cacheable
    // numeric connection directories disappear as daemon connections die.
    mount_at(
        fs,
        FsPath::new(b"/sys/fs/fuse/connections"),
        dev::fuse::new_fusectl(),
    )?;
    mount_at(
        fs,
        FsPath::new(b"/sys/kernel/tracing"),
        trace::new_tracefs(),
    )?;
    // Linux tooling commonly reaches the same tracefs instance through the
    // debugfs-compatible path.
    mount_at(
        fs,
        FsPath::new(b"/sys/kernel/debug/tracing"),
        trace::new_tracefs(),
    )?;
    mount_at(
        fs,
        FsPath::new(b"/sys/kernel/debug/dri/0"),
        graphics_metrics::new_graphics_metricsfs(),
    )?;

    #[cfg(feature = "dev-log")]
    dev::bind_dev_log(fs, boot_security, unix_namespace).expect("Failed to bind /dev/log");

    Ok(())
}

#[cfg(test)]
mod tests {
    use axerrno::{AxError, LinuxError};

    use super::is_missing_path_error;

    #[test]
    fn only_not_found_errors_allow_directory_creation() {
        assert!(is_missing_path_error(AxError::NotFound));
        assert!(is_missing_path_error(AxError::from(LinuxError::ENOENT)));
        assert!(!is_missing_path_error(AxError::from(LinuxError::EIO)));
        assert!(!is_missing_path_error(AxError::from(LinuxError::EEXIST)));
    }
}
