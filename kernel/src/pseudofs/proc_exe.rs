//! Executable links retain the admitted inode, never re-resolve display text.
use alloc::{borrow::Cow, sync::Arc, vec::Vec};

use axfs_ng_vfs::{FsPathBuf, Location, VfsError, VfsResult};
use axtask::{AxTaskRef, WeakAxTaskRef};

use super::{SimpleFile, SimpleFileOps, SimpleFs, proc::proc_image_access};
use crate::task::AsThread;

pub(super) fn link(
    fs: Arc<SimpleFs>,
    task: &AxTaskRef,
    process_view: bool,
) -> VfsResult<Arc<SimpleFile>> {
    drop(proc_image_access(task, process_view)?);
    SimpleFile::try_new_magic_link(
        fs,
        ExecutableLink {
            task: Arc::downgrade(task),
            process_view,
        },
    )
}

struct ExecutableLink {
    task: WeakAxTaskRef,
    process_view: bool,
}

impl ExecutableLink {
    fn snapshot(&self) -> VfsResult<(Location, FsPathBuf)> {
        let task = self.task.upgrade().ok_or(VfsError::NotFound)?;
        let image = proc_image_access(&task, self.process_view)?.into_aspace();
        let process = &task.as_thread().proc_data;
        if process.exec_in_progress() {
            return Err(VfsError::PermissionDenied);
        }
        let location = process.executable_location().ok_or(VfsError::NotFound)?;
        let path = process.try_exe_path()?;
        if process.exec_in_progress() || !process.image_matches(&image) {
            return Err(VfsError::PermissionDenied);
        }
        Ok((location, path))
    }
}

fn link_text(location: &Location, path: &axfs_ng_vfs::FsPath) -> VfsResult<Vec<u8>> {
    let deleted = location.metadata()?.nlink == 0;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(path.as_bytes().len() + if deleted { b" (deleted)".len() } else { 0 })
        .map_err(|_| VfsError::NoMemory)?;
    bytes.extend_from_slice(path.as_bytes());
    if deleted {
        bytes.extend_from_slice(b" (deleted)");
    }
    Ok(bytes)
}

impl SimpleFileOps for ExecutableLink {
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        let (location, path) = self.snapshot()?;
        Ok(Cow::Owned(link_text(&location, &path)?))
    }
    fn write_all(&self, _data: &[u8]) -> VfsResult<()> {
        Err(VfsError::PermissionDenied)
    }
    fn magic_link_target(&self) -> Option<VfsResult<Location>> {
        Some(self.snapshot().map(|(location, _)| location))
    }
}

#[cfg(test)]
mod tests {
    use axfs_ng_vfs::{FsPath, Mountpoint, NodePermission, NodeType};

    use super::*;
    use crate::pseudofs::tmp::MemoryFs;

    #[test]
    fn executable_link_marks_unlinked_inode_without_changing_display_name() {
        let fs = MemoryFs::new().unwrap();
        let root = Mountpoint::new_root(&fs).root_location();
        let name = axfs_ng_vfs::FsName::new(b"elf");
        let file = root
            .create(
                name,
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(0o755),
            )
            .unwrap();
        assert_eq!(link_text(&file, FsPath::new(b"/elf")).unwrap(), b"/elf");
        root.unlink(name, false).unwrap();
        assert_eq!(
            link_text(&file, FsPath::new(b"/elf")).unwrap(),
            b"/elf (deleted)"
        );
        assert_eq!(file.metadata().unwrap().node_type, NodeType::RegularFile);
    }
}
