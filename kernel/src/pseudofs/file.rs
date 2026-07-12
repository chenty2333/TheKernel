use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{any::Any, cmp::Ordering, task::Context};

use axfs_ng_vfs::{
    FileNodeOps, FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission,
    NodeType, NodeUserData, VfsError, VfsResult,
};
use axpoll::{IoEvents, Pollable};
use inherit_methods_macro::inherit_methods;

use super::fs::{SimpleFs, SimpleFsNode};

/// Operations for a simple file.
pub trait SimpleFileOps: Send + Sync + 'static {
    /// Reads all content in the file.
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>>;
    /// Replaces the file's content with `data`.
    fn write_all(&self, data: &[u8]) -> VfsResult<()>;
}

/// Type representing operation applied to a simple file.
pub enum SimpleFileOperation<'a> {
    /// Reading the file's content
    Read,
    /// Replacing the file's content
    Write(&'a [u8]),
}

/// A wrapper that implements [`SimpleFileOps`] for `Fn(SimpleFileOperation) ->
/// VfsResult<Option<impl Into<Vec<u8>>>>`.
pub struct RwFile<F>(F);

impl<F, R> RwFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync,
    R: Into<Vec<u8>>,
{
    /// Creates a new `RwFile`.
    pub fn new(imp: F) -> Self {
        Self(imp)
    }
}

impl<F, R> SimpleFileOps for RwFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        (self.0)(SimpleFileOperation::Read).and_then(|value| {
            value
                .map(|value| Cow::Owned(value.into()))
                .ok_or(VfsError::InvalidData)
        })
    }

    fn write_all(&self, data: &[u8]) -> VfsResult<()> {
        (self.0)(SimpleFileOperation::Write(data)).map(|_| ())
    }
}

impl<F, R> SimpleFileOps for F
where
    F: Fn() -> VfsResult<R> + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        (self)().map(|it| Cow::Owned(it.into()))
    }

    fn write_all(&self, _data: &[u8]) -> VfsResult<()> {
        Err(VfsError::BadFileDescriptor)
    }
}

/// A simple file.
pub struct SimpleFile {
    node: SimpleFsNode,
    ops: Arc<dyn SimpleFileOps>,
    flags: NodeFlags,
    user_data: NodeUserData,
}

impl SimpleFile {
    /// Creates a simple file from given file operations.
    pub fn new(fs: Arc<SimpleFs>, ty: NodeType, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new_with_permission(fs, ty, NodePermission::default(), ops)
    }

    /// Creates a simple file from given file operations and permissions.
    pub fn new_with_permission(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        Self::new_with_permission_and_flags(fs, ty, permission, NodeFlags::NON_CACHEABLE, ops)
    }

    fn new_with_permission_and_flags(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        flags: NodeFlags,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        let node = SimpleFsNode::new(fs, ty, permission);
        Arc::new(Self {
            node,
            ops: Arc::new(ops),
            flags,
            user_data: NodeUserData::new(),
        })
    }

    fn try_new_with_permission_and_flags(
        fs: Arc<SimpleFs>,
        ty: NodeType,
        permission: NodePermission,
        flags: NodeFlags,
        ops: impl SimpleFileOps,
    ) -> VfsResult<Arc<Self>> {
        let ops: Arc<dyn SimpleFileOps> = Arc::try_new(ops).map_err(|_| VfsError::NoMemory)?;
        let node = SimpleFsNode::try_new(fs, ty, permission)?;
        Arc::try_new(Self {
            node,
            ops,
            flags,
            user_data: NodeUserData::new(),
        })
        .map_err(|_| VfsError::NoMemory)
    }

    /// Creates a dynamic link that pathwalk policy can distinguish from an
    /// ordinary filesystem symlink.
    pub fn new_magic_link(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new_with_permission_and_flags(
            fs,
            NodeType::Symlink,
            NodePermission::default(),
            NodeFlags::NON_CACHEABLE | NodeFlags::MAGIC_LINK,
            ops,
        )
    }

    /// Creates a userspace-triggered magic link with fallible inode, operation
    /// object, and file-node publication allocations.
    pub fn try_new_magic_link(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> VfsResult<Arc<Self>> {
        Self::try_new_with_permission_and_flags(
            fs,
            NodeType::Symlink,
            NodePermission::default(),
            NodeFlags::NON_CACHEABLE | NodeFlags::MAGIC_LINK,
            ops,
        )
    }

    /// Creates a simple file from given file operations.
    pub fn new_regular(fs: Arc<SimpleFs>, ops: impl SimpleFileOps) -> Arc<Self> {
        Self::new(fs, NodeType::RegularFile, ops)
    }

    /// Fallibly creates a dynamic regular file whose reads/writes consume the
    /// immutable credential stored in its open file description.
    pub fn try_new_regular_with_open_credential(
        fs: Arc<SimpleFs>,
        ops: impl SimpleFileOps,
    ) -> VfsResult<Arc<Self>> {
        Self::try_new_with_permission_and_flags(
            fs,
            NodeType::RegularFile,
            NodePermission::default(),
            NodeFlags::NON_CACHEABLE | NodeFlags::OPEN_CREDENTIAL,
            ops,
        )
    }

    /// Creates a regular file from given file operations and permissions.
    pub fn new_regular_with_permission(
        fs: Arc<SimpleFs>,
        permission: NodePermission,
        ops: impl SimpleFileOps,
    ) -> Arc<Self> {
        Self::new_with_permission(fs, NodeType::RegularFile, permission, ops)
    }
}

#[inherit_methods(from = "self.node")]
impl NodeOps for SimpleFile {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata> {
        // Dynamic pseudo-files are looked up via metadata() before every open.
        // Recomputing size there would force read_all() during lookup and then
        // again during the actual read, which is prohibitively expensive for
        // hot procfs paths such as /proc/[pid]/stat.
        Ok(self.node.metadata.lock().clone())
    }

    fn len(&self) -> VfsResult<u64> {
        let len = self.ops.read_all()?.len() as u64;
        self.node.metadata.lock().size = len;
        Ok(len)
    }

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, data_only: bool) -> VfsResult<()>;

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn flags(&self) -> NodeFlags {
        self.flags
    }

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.user_data)
    }
}

impl FileNodeOps for SimpleFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let data = self.ops.read_all()?;
        self.node.metadata.lock().size = data.len() as u64;
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let data = &data[offset as usize..];
        let read = data.len().min(buf.len());
        buf[..read].copy_from_slice(&data[..read]);
        Ok(read)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        // Proc-style control files treat each write from offset 0 as a full
        // value replacement instead of patching the previous text contents.
        if offset == 0 {
            self.ops.write_all(buf)?;
            self.node.metadata.lock().size = buf.len() as u64;
            return Ok(buf.len());
        }
        let data = self.ops.read_all()?;
        let mut data = data.to_vec();
        let end_pos = offset + buf.len() as u64;
        if end_pos > data.len() as u64 {
            data.resize(end_pos as usize, 0);
        }
        data[offset as usize..end_pos as usize].copy_from_slice(buf);
        self.ops.write_all(&data)?;
        self.node.metadata.lock().size = data.len() as u64;
        Ok(buf.len())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        let mut data = self.ops.read_all()?.to_vec();
        data.extend_from_slice(buf);
        self.ops.write_all(&data)?;
        self.node.metadata.lock().size = data.len() as u64;
        Ok((buf.len(), data.len() as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        let data = self.ops.read_all()?;
        match len.cmp(&(data.len() as u64)) {
            Ordering::Less => {
                self.ops.write_all(&data[..len as usize])?;
                self.node.metadata.lock().size = len;
                Ok(())
            }
            Ordering::Greater => {
                let mut data = data.to_vec();
                data.resize(len as usize, 0);
                self.ops.write_all(&data)?;
                self.node.metadata.lock().size = len;
                Ok(())
            }
            _ => {
                self.node.metadata.lock().size = len;
                Ok(())
            }
        }
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        self.ops.write_all(target.as_bytes())
    }
}

impl Pollable for SimpleFile {
    fn poll(&self) -> IoEvents {
        IoEvents::READABLE | IoEvents::WRITABLE
    }

    fn register<'a>(
        &'a self,
        _context: &mut Context<'_>,
        _events: IoEvents,
    ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
        axpoll::PollRegistration::empty()
    }
}
