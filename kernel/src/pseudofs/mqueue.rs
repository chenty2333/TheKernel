//! Namespace-bound POSIX message-queue filesystem.

use alloc::{borrow::Cow, sync::Arc};
use core::{any::Any, task::Context};

use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNodeOps, FileNode,
    FileNodeOps, Filesystem, FilesystemOps, FsName, Location, Metadata, MetadataUpdate,
    NamedCreateOptions, NodeFlags, NodeOps, NodePermission, NodeType, Reference, RenameRequest,
    UnlinkRequest, VfsError, VfsResult, WeakDirEntry,
    path::{DOT, DOTDOT},
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use inherit_methods_macro::inherit_methods;
#[cfg(not(test))]
use axsync::Mutex;
#[cfg(test)]
use spin::Mutex;

use crate::{
    pseudofs::{ChildNames, NodeOpsMux, SimpleDirOps, SimpleFs, SimpleFsNode, try_boxed_names},
    syscall::ipc::{
        IpcNamespace, MqReadiness, PosixMqueue, mqueuefs_lookup, mqueuefs_metadata, mqueuefs_names,
        mqueuefs_poll, mqueuefs_read, mqueuefs_readiness, mqueuefs_unlink, mqueuefs_write,
    },
};

const MQUEUE_MAGIC: u32 = 0x1980_0202;

pub(crate) fn new_mqueuefs(namespace: Arc<IpcNamespace>) -> Filesystem {
    SimpleFs::new_with("mqueue".into(), MQUEUE_MAGIC, move |fs| {
        MqueueDir::new_maker(fs, namespace.clone())
    })
}

/// Resolve the queue core behind an ordinary VFS open.  This is the bridge
/// that makes mqueuefs opens and mq_open descriptors name the same queue
/// object rather than two incompatible descriptor families.
pub(crate) fn queue_for_location(location: &Location) -> Option<Arc<Mutex<PosixMqueue>>> {
    location
        .entry()
        .downcast::<MqueueFile>()
        .ok()
        .map(|file| file.queue.clone())
}

struct MqueueDir {
    fs: Arc<SimpleFs>,
    namespace: Arc<IpcNamespace>,
    this: WeakDirEntry,
    node: SimpleFsNode,
}

impl MqueueDir {
    fn new_maker(fs: Arc<SimpleFs>, namespace: Arc<IpcNamespace>) -> crate::pseudofs::DirMaker {
        Arc::new(move |this| {
            Arc::new(Self {
                node: SimpleFsNode::new(
                    fs.clone(),
                    NodeType::Directory,
                    NodePermission::from_bits_truncate(0o1777),
                ),
                fs: fs.clone(),
                namespace: namespace.clone(),
                this,
            })
        })
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for MqueueDir {
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata>;
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, data_only: bool) -> VfsResult<()>;
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl SimpleDirOps for MqueueDir {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        let names = mqueuefs_names(&self.namespace).map_err(VfsError::from)?;
        try_boxed_names(names.into_iter().map(|name| Cow::Owned(name)))
    }
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        let name = axfs_ng_vfs::FsNameBuf::from_vec(name.as_bytes().to_vec())?;
        let queue = mqueuefs_lookup(&self.namespace, &name).map_err(VfsError::from)?;
        MqueueFile::try_new(self.fs.clone(), queue).map(|file| NodeOpsMux::File(file))
    }
    fn is_cacheable(&self) -> bool {
        false
    }
}

impl DirNodeOps for MqueueDir {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let entry = self.this.upgrade().ok_or(VfsError::NotFound)?;
        let directory = entry.as_dir()?;
        let children = [DOT, DOTDOT]
            .into_iter()
            .map(Cow::Borrowed)
            .chain(self.child_names()?);
        let mut count = 0;
        for (index, name) in children.enumerate().skip(offset as usize) {
            let metadata = match name.as_ref() {
                name if name == DOT => entry.metadata(),
                name if name == DOTDOT => entry
                    .parent()
                    .map_or_else(|| entry.metadata(), |parent| parent.metadata()),
                child => directory.lookup(child)?.metadata(),
            }?;
            if !sink.accept(&name, metadata.inode, metadata.node_type, index as u64 + 1) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let NodeOpsMux::File(file) = self.lookup_child(name)? else {
            return Err(VfsError::NotFound);
        };
        let reference = Reference::try_new(self.this.upgrade(), name)?;
        let node_type = file.metadata()?.node_type;
        DirEntry::try_new_file(FileNode::new(file), node_type, reference)
    }
    fn is_cacheable(&self) -> bool {
        false
    }
    fn create_named(
        &self,
        _name: &FsName,
        _options: &NamedCreateOptions,
        _disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        // POSIX queues are published exclusively by mq_open(), which owns
        // queue attributes and the namespace transaction.
        Err(VfsError::OperationNotPermitted)
    }
    fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }
    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        if request.is_dir {
            return Err(VfsError::NotADirectory);
        }
        let name = axfs_ng_vfs::FsNameBuf::from_vec(request.name.as_bytes().to_vec())?;
        mqueuefs_unlink(&self.namespace, &name).map_err(VfsError::from)
    }
    fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

struct MqueueFile {
    node: SimpleFsNode,
    queue: Arc<Mutex<PosixMqueue>>,
    readiness: Arc<MqReadiness>,
}
impl MqueueFile {
    fn try_new(fs: Arc<SimpleFs>, queue: Arc<Mutex<PosixMqueue>>) -> VfsResult<Arc<Self>> {
        let (mode, uid, gid, size) = mqueuefs_metadata(&queue);
        let node = SimpleFsNode::try_new(
            fs,
            NodeType::RegularFile,
            NodePermission::from_bits_truncate((mode & 0o777) as u16),
        )?;
        {
            let mut metadata = node.metadata.lock();
            metadata.uid = uid;
            metadata.gid = gid;
            metadata.size = size;
        }
        let readiness = mqueuefs_readiness(&queue);
        Arc::try_new(Self {
            node,
            queue,
            readiness,
        })
        .map_err(|_| VfsError::NoMemory)
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for MqueueFile {
    fn inode(&self) -> u64;
    fn metadata(&self) -> VfsResult<Metadata>;
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;
    fn filesystem(&self) -> &dyn FilesystemOps;
    fn sync(&self, data_only: bool) -> VfsResult<()>;
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE | NodeFlags::STREAM | NodeFlags::NO_SEEK
    }
}
impl FileNodeOps for MqueueFile {
    fn read_at(&self, destination: &mut [u8], _offset: u64) -> VfsResult<usize> {
        mqueuefs_read(&self.queue, destination).map_err(VfsError::from)
    }
    fn write_at(&self, source: &[u8], _offset: u64) -> VfsResult<usize> {
        mqueuefs_write(&self.queue, source).map_err(VfsError::from)
    }
    fn append(&self, source: &[u8]) -> VfsResult<(usize, u64)> {
        self.write_at(source, 0).map(|count| (count, 0))
    }
    fn set_len(&self, _len: u64) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
    fn set_symlink(&self, _target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        Err(VfsError::InvalidInput)
    }
}
impl Pollable for MqueueFile {
    fn poll(&self) -> IoEvents {
        mqueuefs_poll(&self.queue)
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        if events.intersects(IoEvents::READABLE | IoEvents::WRITABLE) {
            PollRegistration::single(
                if events.intersects(IoEvents::READABLE) {
                    &self.readiness.readable
                } else {
                    &self.readiness.writable
                },
                context.waker(),
            )
        } else {
            PollRegistration::empty()
        }
    }
}
