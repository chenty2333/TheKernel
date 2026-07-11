use alloc::{
    borrow::{Cow, ToOwned},
    boxed::Box,
    collections::btree_map::BTreeMap,
    string::String,
    sync::Arc,
};
use core::any::Any;

use axfs_ng_vfs::{
    CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileNode,
    FilesystemOps, Metadata, MetadataUpdate, NamedCreateOptions, NodeOps, NodePermission, NodeType,
    Reference, RenameRequest, UnlinkRequest, VfsError, VfsResult, WeakDirEntry,
    path::{DOT, DOTDOT},
};
use inherit_methods_macro::inherit_methods;

use super::{DirMaker, NodeOpsMux, SimpleFs, SimpleFsNode};

/// Fallible owned iterator returned by simple pseudo-directory enumerators.
pub type ChildNames<'a> = Box<dyn Iterator<Item = Cow<'a, str>> + 'a>;

/// Boxes a directory-name iterator without invoking the infallible allocation
/// path from a user-triggered `getdents` operation.
pub fn try_boxed_names<'a>(
    names: impl Iterator<Item = Cow<'a, str>> + 'a,
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
    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux>;

    /// Check if the directory is cacheable.
    ///
    /// See [`DirNodeOps::is_cacheable`].
    fn is_cacheable(&self) -> bool {
        true
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
        try_boxed_names(self.0.keys().map(|s| s.as_str().into()))
    }

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
        self.0.get(name).cloned().ok_or(VfsError::NotFound)
    }
}

/// A mapping of directory names to entries.
pub struct DirMapping(BTreeMap<String, NodeOpsMux>);

impl DirMapping {
    /// Create a new empty directory mapping.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Add a new entry to the directory mapping.
    pub fn add(&mut self, name: impl Into<String>, ops: impl Into<NodeOpsMux>) {
        self.0.insert(name.into(), ops.into());
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

    fn lookup_child(&self, name: &str) -> VfsResult<NodeOpsMux> {
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
                DOT => this_entry.metadata(),
                DOTDOT => this_entry
                    .parent()
                    .map_or_else(|| this_entry.metadata(), |parent| parent.metadata()),
                other => {
                    let entry = this_dir.lookup(other)?;
                    entry.metadata()
                }
            }?;
            if !sink.accept(&name, metadata.inode, metadata.node_type, i as u64 + 1) {
                break;
            }
            count += 1;
        }

        Ok(count)
    }

    fn lookup(&self, name: &str) -> VfsResult<DirEntry> {
        let ops = self.ops.lookup_child(name)?;
        let reference = Reference::new(self.this.upgrade(), name.to_owned());
        Ok(match ops {
            NodeOpsMux::Dir(maker) => {
                DirEntry::new_dir(|this| DirNode::new(maker(this)), reference)
            }
            NodeOpsMux::File(ops) => {
                let node_type = ops.metadata()?.node_type;
                DirEntry::new_file(FileNode::new(ops.clone()), node_type, reference)
            }
        })
    }

    fn is_cacheable(&self) -> bool {
        self.ops.is_cacheable()
    }

    fn create_named(
        &self,
        _name: &str,
        _options: &NamedCreateOptions,
        _disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        Err(VfsError::OperationNotPermitted)
    }

    fn link(&self, _name: &str, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(VfsError::OperationNotPermitted)
    }

    fn unlink(&self, _request: UnlinkRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }

    fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
        Err(VfsError::OperationNotPermitted)
    }
}
