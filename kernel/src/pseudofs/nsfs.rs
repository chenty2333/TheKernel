//! A private namespace filesystem; proc links jump to actual retained objects.
use alloc::{string::String, sync::Arc, vec::Vec};
use core::{any::Any, fmt::Write, task::Context};

use axfs_ng_vfs::{
    DirEntry, FileNode, FileNodeOps, FilesystemOps, FsName, FsPath, FsPathBuf, Location, Metadata,
    MetadataUpdate, Mountpoint, NodeFlags, NodeOps, NodePermission, NodeType, NodeUserData,
    Reference, VfsError, VfsResult, WeakDirEntry,
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axtask::{AxTaskRef, WeakAxTaskRef};
use hashbrown::HashMap;
use inherit_methods_macro::inherit_methods;
use spin::{Mutex, Once};

use super::{
    DirMapping, NodeOpsMux, SimpleDir, SimpleFs, SimpleFsNode,
    proc::{
        ProcNamespaceFile, ProcNamespaceKind, ProcNamespaceObject, ProcNamespaceTarget,
        namespace_target_from_proc_file, proc_fd_image_access, validate_proc_fd_image,
    },
};
use crate::task::AsThread;

const NSFS_MAGIC: u32 = 0x6e73_6673;
struct NamespaceFs {
    fs: Arc<SimpleFs>,
    mount: Arc<Mountpoint>,
    entries: Mutex<HashMap<u64, WeakDirEntry>>,
}
static NAMESPACE_FS: Once<NamespaceFs> = Once::new();

fn provider() -> &'static NamespaceFs {
    NAMESPACE_FS.call_once(|| {
        let mut operations = None;
        let fs = SimpleFs::new_with("nsfs".into(), NSFS_MAGIC, |fs| {
            operations = Some(fs.clone());
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        NamespaceFs {
            fs: operations.unwrap(),
            mount: Mountpoint::new_root(&fs),
            entries: Mutex::new(HashMap::new()),
        }
    })
}

/// This one hidden kernel mount is not a member of any user mount graph.
/// Unknown ordinary mounts must not gain its policy exception.
pub(crate) fn is_internal_mount(mount: &Arc<Mountpoint>) -> bool {
    NAMESPACE_FS
        .get()
        .is_some_and(|fs| Arc::ptr_eq(&fs.mount, mount))
}

fn label(kind: ProcNamespaceKind, inode: u64) -> VfsResult<Vec<u8>> {
    let prefix = match kind {
        ProcNamespaceKind::Cgroup => "cgroup",
        ProcNamespaceKind::Ipc => "ipc",
        ProcNamespaceKind::Mount => "mnt",
        ProcNamespaceKind::Net => "net",
        ProcNamespaceKind::Pid => "pid",
        ProcNamespaceKind::Time | ProcNamespaceKind::TimeForChildren => "time",
        ProcNamespaceKind::User => "user",
        ProcNamespaceKind::Uts => "uts",
    };
    let mut text = String::new();
    text.try_reserve(64).map_err(|_| VfsError::NoMemory)?;
    // The largest u64 inode and longest fixed prefix fit in reserved storage.
    write!(text, "{prefix}:[{inode}]").map_err(|_| VfsError::Io)?;
    Ok(text.into_bytes())
}

fn retain_location(file: Arc<ProcNamespaceFile>, kind: ProcNamespaceKind) -> VfsResult<Location> {
    let fs = provider();
    let inode = file.namespace_inode().ok_or(VfsError::InvalidData)?;
    let mut entries = fs.entries.lock();
    entries.retain(|_, entry| entry.upgrade().is_some());
    if let Some(entry) = entries.get(&inode).and_then(WeakDirEntry::upgrade) {
        return Ok(Location::new(fs.mount.clone(), entry));
    }
    let name = label(kind, inode)?;
    let entry = DirEntry::try_new_file(
        FileNode::new(file),
        NodeType::RegularFile,
        Reference::try_new(
            Some(fs.mount.root_location().entry().clone()),
            FsName::new(&name),
        )?,
    )?;
    entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    entries.insert(inode, entry.downgrade());
    Ok(Location::new(fs.mount.clone(), entry))
}

pub(super) fn object_location(
    kind: ProcNamespaceKind,
    object: ProcNamespaceObject,
) -> VfsResult<Location> {
    retain_location(
        ProcNamespaceFile::from_object(provider().fs.clone(), kind, object)?,
        kind,
    )
}

pub(crate) fn descriptor_label(loc: &Location) -> Option<VfsResult<FsPathBuf>> {
    if !is_internal_mount(loc.mountpoint()) {
        return None;
    }
    let ProcNamespaceTarget::Live(kind, _) = namespace_target_from_proc_file(loc) else {
        return None;
    };
    Some(label(kind, loc.inode()).map(FsPathBuf::from_vec))
}

pub(super) fn proc_link(
    fs: Arc<SimpleFs>,
    kind: ProcNamespaceKind,
    task: &AxTaskRef,
    process_view: bool,
) -> VfsResult<NodeOpsMux> {
    let node = SimpleFsNode::try_new(
        fs,
        NodeType::Symlink,
        NodePermission::from_bits_truncate(0o777),
    )?;
    let ids = task.as_thread().current_cred().ids();
    {
        let mut metadata = node.metadata.lock();
        metadata.uid = ids.euid.into_raw();
        metadata.gid = ids.egid.into_raw();
    }
    Ok(Arc::try_new(NamespaceLink {
        node,
        task: Arc::downgrade(task),
        kind,
        process_view,
    })
    .map_err(|_| VfsError::NoMemory)?
    .into())
}

struct NamespaceLink {
    node: SimpleFsNode,
    task: WeakAxTaskRef,
    kind: ProcNamespaceKind,
    process_view: bool,
}

impl NamespaceLink {
    fn target(&self) -> VfsResult<Location> {
        let task = self.task.upgrade().ok_or(VfsError::PermissionDenied)?;
        let image = proc_fd_image_access(&task, self.process_view)?;
        let file =
            ProcNamespaceFile::new(provider().fs.clone(), self.kind, &task, self.process_view)?;
        let location = retain_location(file, self.kind)?;
        validate_proc_fd_image(&task, &image)?;
        Ok(location)
    }
    fn text(&self) -> VfsResult<FsPathBuf> {
        let target = self.target()?;
        label(self.kind, target.inode()).map(FsPathBuf::from_vec)
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for NamespaceLink {
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.node.user_data)
    }
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata>;
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }
    fn len(&self) -> VfsResult<u64> {
        Ok(self.text()?.as_bytes().len() as u64)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::MAGIC_LINK
    }
    fn magic_link_target(&self) -> Option<VfsResult<Location>> {
        Some(self.target())
    }
    fn read_link_text(&self) -> Option<VfsResult<FsPathBuf>> {
        Some(self.text())
    }
}
impl FileNodeOps for NamespaceLink {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let text = self.text()?;
        if offset >= text.as_bytes().len() as u64 {
            return Ok(0);
        }
        let bytes = &text.as_bytes()[offset as usize..];
        let count = bytes.len().min(buf.len());
        buf[..count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Err(VfsError::BadFileDescriptor)
    }
    fn append(&self, _buf: &[u8]) -> VfsResult<(usize, u64)> {
        Err(VfsError::BadFileDescriptor)
    }
    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
    fn set_symlink(&self, _target: &FsPath) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}
impl Pollable for NamespaceLink {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }
    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        PollRegistration::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::UserNamespace;

    #[test]
    fn labels_use_object_ids_and_real_time_type_not_source_alias() {
        assert_eq!(label(ProcNamespaceKind::Net, 42).unwrap(), b"net:[42]");
        assert_eq!(
            label(ProcNamespaceKind::TimeForChildren, u64::MAX).unwrap(),
            b"time:[18446744073709551615]"
        );
    }
    #[test]
    fn namespace_provider_reuses_live_inode_and_has_distinct_real_filesystem() {
        let _context = crate::test_support::scheduler_test_context();
        let user = UserNamespace::try_new_root().unwrap();
        let first = object_location(
            ProcNamespaceKind::User,
            ProcNamespaceObject::User(user.clone()),
        )
        .unwrap();
        let second =
            object_location(ProcNamespaceKind::User, ProcNamespaceObject::User(user)).unwrap();
        assert!(first.entry().ptr_eq(second.entry()));
        assert!(is_internal_mount(first.mountpoint()));
        assert_eq!(first.filesystem().stat().unwrap().fs_type, NSFS_MAGIC);
        assert_eq!(
            descriptor_label(&first).unwrap().unwrap().as_bytes(),
            label(ProcNamespaceKind::User, first.inode()).unwrap()
        );
        let ordinary = SimpleFs::new_with("ordinary".into(), 1, |fs| {
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        let ordinary = Mountpoint::new_root(&ordinary).root_location();
        assert_ne!(
            first.metadata().unwrap().device,
            ordinary.metadata().unwrap().device
        );
        assert!(!is_internal_mount(ordinary.mountpoint()));
    }
}
