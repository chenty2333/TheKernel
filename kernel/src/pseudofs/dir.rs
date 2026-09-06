use alloc::{borrow::Cow, boxed::Box, collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::any::Any;

use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode,
    FilesystemOps, FsName, FsNameBuf, Metadata, MetadataUpdate, NamedCreateOptions, NodeOps,
    NodePermission, NodeType, NodeUserData, Reference, RenameRequest, UnlinkRequest, VfsError,
    VfsResult, WeakDirEntry,
    path::{DOT, DOTDOT},
};
use inherit_methods_macro::inherit_methods;

use super::{DirMaker, NodeOpsMux, SimpleFs, SimpleFsNode};

/// Fallible owned iterator returned by simple pseudo-directory enumerators.
pub type ChildNames<'a> = Box<dyn Iterator<Item = Cow<'a, FsName>> + 'a>;

/// Boxes a directory-name iterator without invoking the infallible allocation
/// path from a user-triggered `getdents` operation.
pub fn try_boxed_names<'a>(
    names: impl Iterator<Item = Cow<'a, FsName>> + 'a,
) -> VfsResult<ChildNames<'a>> {
    Box::try_new(names)
        .map(|names| names as ChildNames<'a>)
        .map_err(|_| VfsError::NoMemory)
}

/// Operations for a simple directory.
pub trait SimpleDirOps: Send + Sync + 'static {
    /// Get the names of all children in the directory.
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>>;
    /// Look up a child directory or file by name.
    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux>;

    /// Check if the directory is cacheable.
    ///
    /// See [`DirNodeOps::is_cacheable`].
    fn is_cacheable(&self) -> bool {
        true
    }

    /// Returns the current namespace generation for dynamic directories.
    /// Static pseudo-directories retain the zero default.
    fn namespace_epoch(&self) -> u64 {
        0
    }

    /// Returns whether this directory can publish a named inode of `node_type`.
    fn supports_named_create(&self, _node_type: NodeType) -> bool {
        false
    }

    /// Atomically publishes a named entry. The default keeps static pseudo-
    /// directories fail-closed; dynamic providers may use `parent` to build
    /// the exact dentry before exposing their new name.
    fn create_named(
        &self,
        _parent: Option<DirEntry>,
        _name: &FsName,
        _options: &NamedCreateOptions,
        _disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Returns whether this directory supports unlinking a named non-directory.
    fn supports_unlink(&self) -> bool {
        false
    }

    /// Removes one named entry. The default keeps static pseudo-directories
    /// immutable.
    fn unlink(&self, _request: UnlinkRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    /// Combines two directories into one.
    fn chain<N: SimpleDirOps>(self, other: N) -> ChainedDirOps<Self, N>
    where
        Self: Sized,
    {
        ChainedDirOps(self, other)
    }
}

impl SimpleDirOps for DirMapping {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        try_boxed_names(self.0.keys().map(|name| Cow::Borrowed(name.as_name())))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        self.0.get(name).cloned().ok_or(VfsError::NotFound)
    }
}

/// A mapping of directory names to entries.
pub struct DirMapping(BTreeMap<FsNameBuf, NodeOpsMux>);

impl DirMapping {
    /// Create a new empty directory mapping.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Add a new entry to the directory mapping.
    pub fn add(&mut self, name: impl AsRef<[u8]>, ops: impl Into<NodeOpsMux>) {
        let name = FsNameBuf::from_vec(Vec::from(name.as_ref()))
            .expect("simple pseudo-filesystem entry names must be valid");
        self.0.insert(name, ops.into());
    }
}

impl Default for DirMapping {
    fn default() -> Self {
        Self::new()
    }
}

/// Directory created by [`SimpleDirOps::chain`].
pub struct ChainedDirOps<A, B>(A, B);

impl<A: SimpleDirOps, B: SimpleDirOps> SimpleDirOps for ChainedDirOps<A, B> {
    fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
        try_boxed_names(self.0.child_names()?.chain(self.1.child_names()?))
    }

    fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
        match self.0.lookup_child(name) {
            Ok(ops) => Ok(ops),
            Err(VfsError::NotFound) => self.1.lookup_child(name),
            Err(e) => Err(e),
        }
    }

    fn is_cacheable(&self) -> bool {
        // TODO: If one of the ops is not cacheable while the other is, the
        // behavior is undefined.
        self.0.is_cacheable() && self.1.is_cacheable()
    }
}

/// Simple directory.
pub struct SimpleDir<O> {
    node: SimpleFsNode,
    this: WeakDirEntry,
    ops: Arc<O>,
}

impl<O: SimpleDirOps> SimpleDir<O> {
    fn new(node: SimpleFsNode, ops: Arc<O>, this: WeakDirEntry) -> Arc<Self> {
        Arc::new(Self { node, this, ops })
    }

    pub fn ops(&self) -> &Arc<O> {
        &self.ops
    }

    /// Create a [`DirMaker`] from given directory operations.
    pub fn new_maker(fs: Arc<SimpleFs>, ops: Arc<O>) -> DirMaker {
        Arc::new(move |this| {
            SimpleDir::new(
                SimpleFsNode::new(
                    fs.clone(),
                    NodeType::Directory,
                    NodePermission::from_bits_truncate(0o755),
                ),
                ops.clone(),
                this,
            )
        })
    }
}

#[inherit_methods(from = "self.node")]
impl<O: SimpleDirOps> NodeOps for SimpleDir<O> {
    fn inode(&self) -> u64;

    fn metadata(&self) -> VfsResult<Metadata>;

    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()>;

    fn filesystem(&self) -> &dyn FilesystemOps;

    fn sync(&self, data_only: bool) -> VfsResult<()>;

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.node.user_data)
    }
}

impl<O: SimpleDirOps> DirNodeOps for SimpleDir<O> {
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let children = [DOT, DOTDOT]
            .into_iter()
            .map(Cow::Borrowed)
            .chain(self.ops.child_names()?);

        let this_entry = self.this.upgrade().ok_or(VfsError::NotFound)?;
        let this_dir = this_entry.as_dir()?;

        let mut count = 0;
        for (i, name) in children.enumerate().skip(offset as usize) {
            let metadata = match name.as_ref() {
                name if name == DOT => this_entry.metadata(),
                name if name == DOTDOT => this_entry
                    .parent()
                    .map_or_else(|| this_entry.metadata(), |parent| parent.metadata()),
                other => {
                    // Dynamic directories (notably procfs) can lose a child
                    // after taking the name snapshot. That child is absent
                    // from this enumeration, not an error for the directory.
                    match this_dir.lookup(other).and_then(|entry| entry.metadata()) {
                        Err(error) if error.canonicalize() == VfsError::NotFound => continue,
                        result => result,
                    }
                }
            }?;
            if !sink.accept(&name, metadata.inode, metadata.node_type, i as u64 + 1) {
                break;
            }
            count += 1;
        }

        Ok(count)
    }

    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let ops = self.ops.lookup_child(name)?;
        let reference = Reference::try_new(self.this.upgrade(), name)?;
        let entry = match ops {
            NodeOpsMux::Dir(maker) => {
                DirEntry::new_dir(|this| DirNode::new(maker(this)), reference)
            }
            NodeOpsMux::File(ops) => {
                let node_type = ops.metadata()?.node_type;
                DirEntry::try_new_file(FileNode::new(ops), node_type, reference)?
            }
        };
        Ok(entry)
    }

    fn is_cacheable(&self) -> bool {
        self.ops.is_cacheable()
    }

    fn namespace_epoch(&self) -> u64 {
        self.ops.namespace_epoch()
    }

    fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.ops.supports_named_create(node_type)
    }

    fn create_named(
        &self,
        name: &FsName,
        options: &NamedCreateOptions,
        disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        self.ops
            .create_named(self.this.upgrade(), name, options, disposition)
    }

    fn supports_unlink(&self) -> bool {
        self.ops.supports_unlink()
    }

    fn unlink(&self, request: UnlinkRequest<'_>) -> VfsResult<()> {
        self.ops.unlink(request)
    }

    fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ChangingChildren {
        fs: Arc<SimpleFs>,
        missing_error: VfsError,
    }

    impl SimpleDirOps for ChangingChildren {
        fn child_names<'a>(&'a self) -> VfsResult<ChildNames<'a>> {
            try_boxed_names([b"gone".as_slice(), b"kept"].into_iter().map(|name| {
                Cow::Borrowed(FsName::new(name))
            }))
        }

        fn lookup_child(&self, name: &FsName) -> VfsResult<NodeOpsMux> {
            if name.as_bytes() == b"gone" {
                return Err(self.missing_error);
            }
            Ok(SimpleDir::new_maker(self.fs.clone(), Arc::new(DirMapping::new())).into())
        }

        fn is_cacheable(&self) -> bool {
            false
        }
    }

    #[test]
    fn enumeration_skips_disappeared_children_without_hiding_other_errors() {
        let _test_context = crate::test_support::scheduler_test_context();
        for missing_error in [
            VfsError::NotFound,
            axerrno::LinuxError::ENOENT.into(),
            VfsError::PermissionDenied,
        ] {
            let filesystem = SimpleFs::new_with("changing-dir-test".into(), 0, move |fs| {
                SimpleDir::new_maker(fs.clone(), Arc::new(ChangingChildren { fs, missing_error }))
            });
            let root = filesystem.root_dir();
            let dir = root.as_dir().unwrap();
            let mut entries = Vec::new();
            let result = dir.read_dir(2, &mut |name: &FsName, _, _, offset| {
                entries.push((name.as_bytes().to_vec(), offset));
                true
            });
            if missing_error.canonicalize() == VfsError::NotFound {
                assert_eq!(result, Ok(1));
                assert_eq!(entries, alloc::vec![(b"kept".to_vec(), 4)]);
            } else {
                assert_eq!(result, Err(missing_error));
                assert!(entries.is_empty());
            }
        }
    }

    #[test]
    fn simple_dir_exposes_a_stable_writeback_error_source() {
        let _test_context = crate::test_support::scheduler_test_context();
        let filesystem = SimpleFs::new_with("simple-dir-test".into(), 0, |fs| {
            SimpleDir::new_maker(fs, Arc::new(DirMapping::new()))
        });
        let dir = filesystem
            .root_dir()
            .as_dir()
            .unwrap()
            .downcast::<SimpleDir<DirMapping>>()
            .unwrap();

        let persistent = dir.persistent_user_data().unwrap();
        let expected = persistent.writeback_error_state().unwrap();
        let actual = dir.writeback_error_state().unwrap();

        assert!(Arc::ptr_eq(&actual, &expected));
        assert!(Arc::ptr_eq(
            &actual,
            &dir.writeback_error_state().unwrap()
        ));
    }
}
