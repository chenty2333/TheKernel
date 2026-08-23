use alloc::{borrow::Cow, sync::Arc, vec::Vec};
use core::{any::Any, task::Context};

use axfs_ng_vfs::{
    FileNodeOps, FilesystemOps, Metadata, MetadataUpdate, NodeFlags, NodeOps, NodePermission,
    NodeType, NodeUserData, VfsError, VfsResult,
};
use axpoll::{IoEvents, Pollable};
use inherit_methods_macro::inherit_methods;
use memory_addr::PAGE_SIZE_4K;

use super::fs::{SimpleFs, SimpleFsNode};

// Linux's proc sysctl write path accepts at most one page minus the trailing
// NUL it adds before dispatching the handler.  SimpleFile is the shared value
// endpoint for those controls, so keep its user-controlled submissions within
// the same bounded shape before a handler can parse or retain them.
const SIMPLE_FILE_MAX_VALUE_LEN: usize = PAGE_SIZE_4K - 1;

fn validate_value_write_len(len: usize) -> VfsResult<()> {
    if len > SIMPLE_FILE_MAX_VALUE_LEN {
        return Err(VfsError::InvalidInput);
    }
    Ok(())
}

/// Operations for a simple file.
pub trait SimpleFileOps: Send + Sync + 'static {
    /// Default inode permissions for a regular file backed by these
    /// operations. Read-only operation implementations must not advertise a
    /// writable inode; writable pseudo-files are owner-writable by default.
    fn default_permission(&self) -> NodePermission {
        NodePermission::from_bits_truncate(0o444)
    }

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
pub struct RwFile<F> {
    imp: F,
    permission: NodePermission,
}

impl<F, R> RwFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync,
    R: Into<Vec<u8>>,
{
    /// Creates an owner-writable `RwFile` suitable for global controls.
    pub fn new(imp: F) -> Self {
        Self {
            imp,
            permission: NodePermission::from_bits_truncate(0o644),
        }
    }

    /// Creates a writable global control file that only root may modify.
    pub fn new_root_writable(imp: F) -> Self {
        Self::new(imp)
    }

    /// Preserves the existing access mode for per-process controls that must
    /// remain writable by their non-root owner. Call sites remain responsible
    /// for target-specific authorization until pseudo-inode ownership exists.
    pub fn new_process_writable(imp: F) -> Self {
        Self {
            imp,
            permission: NodePermission::default(),
        }
    }
}

impl<F, R> SimpleFileOps for RwFile<F>
where
    F: Fn(SimpleFileOperation) -> VfsResult<Option<R>> + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    fn default_permission(&self) -> NodePermission {
        self.permission
    }

    fn read_all(&self) -> VfsResult<Cow<'_, [u8]>> {
        (self.imp)(SimpleFileOperation::Read).and_then(|value| {
            value
                .map(|value| Cow::Owned(value.into()))
                .ok_or(VfsError::InvalidData)
        })
    }

    fn write_all(&self, data: &[u8]) -> VfsResult<()> {
        (self.imp)(SimpleFileOperation::Write(data)).map(|_| ())
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
        let permission = match ty {
            NodeType::RegularFile => ops.default_permission(),
            _ => NodePermission::default(),
        };
        Self::new_with_permission(fs, ty, permission, ops)
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
        let permission = ops.default_permission();
        Self::try_new_with_permission_and_flags(
            fs,
            NodeType::RegularFile,
            permission,
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

    fn write_at(&self, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Proc-style value endpoints ignore the file position: each write is
        // a complete value submission. Never turn the user-controlled offset
        // into a sparse-file allocation. Validate the bounded value shape
        // before dispatching to the handler.
        validate_value_write_len(buf.len())?;
        self.ops.write_all(buf)?;
        self.node.metadata.lock().size = buf.len() as u64;
        Ok(buf.len())
    }

    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        // O_APPEND has the same value-submission semantics and does not
        // concatenate generated contents.
        validate_value_write_len(buf.len())?;
        self.ops.write_all(buf)?;
        self.node.metadata.lock().size = buf.len() as u64;
        Ok((buf.len(), buf.len() as u64))
    }

    fn set_len(&self, len: u64) -> VfsResult<()> {
        // Proc-style value endpoints do not have stored contents to resize.
        // Linux accepts ftruncate/O_TRUNC on these files as a no-op; never
        // synthesize a user-controlled buffer or dispatch a fake write.
        let _ = len;
        Ok(())
    }

    fn set_symlink(&self, target: &str) -> VfsResult<()> {
        self.ops.write_all(target.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::pseudofs::{DirMapping, SimpleDir};

    fn new_test_file(ops: impl SimpleFileOps) -> Arc<SimpleFile> {
        let _test_context = crate::test_support::scheduler_test_context();
        let holder = Arc::new(axsync::Mutex::new(None));
        let holder_for_root = holder.clone();
        let filesystem = SimpleFs::new_with("simple-file-test".into(), 0, move |fs| {
            *holder_for_root.lock() = Some(SimpleFile::new_regular(fs.clone(), ops));
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        drop(filesystem);
        holder.lock().take().unwrap()
    }

    #[test]
    fn default_permissions_match_operation_capability() {
        let read_only = new_test_file(|| Ok::<_, VfsError>(b"value"));
        assert_eq!(read_only.metadata().unwrap().mode.bits(), 0o444);

        let writable = new_test_file(RwFile::new_root_writable(|operation| match operation {
            SimpleFileOperation::Read => Ok(Some(b"value".to_vec())),
            SimpleFileOperation::Write(_) => Ok(None),
        }));
        assert_eq!(writable.metadata().unwrap().mode.bits(), 0o644);

        let process_control =
            new_test_file(RwFile::new_process_writable(|operation| match operation {
                SimpleFileOperation::Read => Ok(Some(b"value".to_vec())),
                SimpleFileOperation::Write(_) => Ok(None),
            }));
        assert_eq!(process_control.metadata().unwrap().mode.bits(), 0o666);
    }

    #[test]
    fn positioned_write_ignores_offset_without_reading_or_allocating() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_for_ops = dispatches.clone();
        let file = new_test_file(RwFile::new(move |operation| {
            dispatches_for_ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                SimpleFileOperation::Read => panic!("positioned write must not read old contents"),
                SimpleFileOperation::Write(data) => {
                    assert_eq!(data, b"x");
                    Ok::<Option<Vec<u8>>, VfsError>(None)
                }
            }
        }));

        assert_eq!(file.write_at(b"x", u64::MAX), Ok(1));
        assert_eq!(dispatches.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn sequential_positioned_writes_each_submit_a_complete_value() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_for_ops = dispatches.clone();
        let file = new_test_file(RwFile::new(move |operation| {
            dispatches_for_ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                SimpleFileOperation::Read => panic!("write must not read old contents"),
                SimpleFileOperation::Write(_) => Ok::<Option<Vec<u8>>, VfsError>(None),
            }
        }));

        assert_eq!(file.write_at(b"first", 0), Ok(5));
        assert_eq!(file.write_at(b"second", 5), Ok(6));
        assert_eq!(dispatches.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn append_submits_value_without_reading_or_concatenating() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_for_ops = dispatches.clone();
        let file = new_test_file(RwFile::new(move |operation| {
            dispatches_for_ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                SimpleFileOperation::Read => panic!("append must not read old contents"),
                SimpleFileOperation::Write(data) => {
                    assert_eq!(data, b"new");
                    Ok::<Option<Vec<u8>>, VfsError>(None)
                }
            }
        }));

        assert_eq!(file.append(b"new"), Ok((3, 3)));
        assert_eq!(dispatches.load(Ordering::Relaxed), 1);
        assert_eq!(file.metadata().unwrap().size, 3);
    }

    #[test]
    fn truncate_is_a_noop_without_dispatch_or_allocation() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_for_ops = dispatches.clone();
        let file = new_test_file(RwFile::new(move |operation| {
            dispatches_for_ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                SimpleFileOperation::Read => Ok(Some(b"value".to_vec())),
                SimpleFileOperation::Write(_) => Ok(None),
            }
        }));

        assert_eq!(file.set_len(u64::MAX), Ok(()));
        assert_eq!(dispatches.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn oversized_write_is_rejected_before_handler_dispatch() {
        let dispatches = Arc::new(AtomicUsize::new(0));
        let dispatches_for_ops = dispatches.clone();
        let file = new_test_file(RwFile::new(move |operation| {
            dispatches_for_ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                SimpleFileOperation::Read => Ok(Some(b"value".to_vec())),
                SimpleFileOperation::Write(_) => Ok(None),
            }
        }));

        let oversized = [0; PAGE_SIZE_4K];
        assert_eq!(file.write_at(&oversized, 0), Err(VfsError::InvalidInput));
        assert_eq!(dispatches.load(Ordering::Relaxed), 0);
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
