//! The immutable `nullfs` filesystem.
//!
//! Linux creates this filesystem exactly once, at boot, so that the initial
//! mount namespace has a permanent, unmountable root that the mutable
//! `rootfs`/`tmpfs` tree is layered on top of:
//!
//! ```c
//! 	inode = new_inode(s);
//! 	...
//! 	/* nullfs is permanently empty... */
//! 	make_empty_dir_inode(inode);
//! 	simple_inode_init_ts(inode);
//! 	inode->i_ino	= 1;
//! 	/* ... and immutable. */
//! 	inode->i_flags |= S_IMMUTABLE;
//! ```
//! (`fs/nullfs.c`:12-34)
//!
//! `nullfs_init_fs_context()` additionally marks it `fc->global = true`,
//! `SB_NOUSER` and `SB_I_NOEXEC | SB_I_NODEV` (`fs/nullfs.c`:58-63), so
//! userspace can neither create a second instance nor write through it.  The
//! single root directory is therefore the whole filesystem: every lookup
//! misses, and every namespace mutation is refused.

use alloc::sync::Arc;
use core::any::Any;

use spin::Once;

use crate::path::{FsName, MAX_NAME_LEN};
use crate::{
    CreateDisposition, CreateOutcome, DirEntry, DirEntrySink, DirNode, DirNodeOps, FileAttr,
    FileAttrProvider, Filesystem, FilesystemOps, Metadata, MetadataUpdate,
    MetadataUpdateCapabilities, NamedCreateOptions, NodeOps, NodePermission, NodeType,
    NodeUserData, Reference, RenameRequest, StatFs, Timestamp, UnlinkRequest, VfsError, VfsResult,
    WritebackErrorState,
};

/// Linux `NULL_FS_MAGIC` (`include/uapi/linux/magic.h`:107).
pub const NULL_FS_MAGIC: u32 = 0x4E55_4C4C;

/// Linux allocates the nullfs root inode as inode 1 (`fs/nullfs.c`:32).
pub const NULLFS_ROOT_INODE: u64 = 1;

/// `make_empty_dir_inode()` gives the root mode `S_IFDIR | 0555`.
const NULLFS_ROOT_MODE: u16 = 0o555;

/// Linux `FS_XFLAG_IMMUTABLE` (`include/uapi/linux/fs.h`): the projection the
/// kernel's `admit_native_namespace_mutation()` tests before any pathname
/// mutation.
const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;

/// `S_IMMUTABLE` makes every namespace mutation fail with `EPERM`; the empty
/// directory has no names, so lookups simply miss.
fn immutable() -> VfsError {
    VfsError::OperationNotPermitted
}

/// Creates the single, global nullfs instance.
///
/// There is deliberately no userspace entry point: Linux `fc->global = true`
/// means `get_tree_single()` reuses this one superblock (`fs/nullfs.c`:41-45, 58-63).
pub fn filesystem() -> VfsResult<Filesystem> {
    Filesystem::try_new(NullFs::new()?)
}

struct NullFs {
    root: Once<DirEntry>,
    /// Linux keeps the superblock's errseq in `sb->s_wb_err`, and the
    /// immutable nullfs root is the whole superblock (`fs/nullfs.c`:12-34),
    /// so node-scoped and `syncfs`-scoped writeback reporting are the same
    /// sequence here.  Without persistent inode data the ordinary
    /// `NodeOps::writeback_error_state()` would answer `EOPNOTSUPP` to every
    /// open of the root, which Linux never does.
    user_data: NodeUserData,
}

impl NullFs {
    fn new() -> VfsResult<Arc<Self>> {
        let filesystem = Arc::try_new(Self {
            root: Once::new(),
            user_data: NodeUserData::new(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let root = DirEntry::new_dir(
            {
                let filesystem = filesystem.clone();
                move |this| {
                    DirNode::new(Arc::new(NullDir {
                        filesystem: filesystem.clone(),
                        this,
                    }))
                }
            },
            Reference::root(),
        );
        filesystem.root.call_once(|| root);
        Ok(filesystem)
    }
}

impl FilesystemOps for NullFs {
    fn name(&self) -> &str {
        "nullfs"
    }

    fn root_dir(&self) -> DirEntry {
        self.root.get().expect("nullfs root dentry").clone()
    }

    fn stat(&self) -> VfsResult<StatFs> {
        Ok(StatFs {
            fs_type: NULL_FS_MAGIC,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 1,
            free_file_count: 0,
            name_length: MAX_NAME_LEN as u32,
            fragment_size: 4096,
            mount_flags: 0,
        })
    }

    /// Everything about nullfs is fixed at `nullfs_fs_fill_super()` time.
    fn metadata_update_capabilities(&self) -> MetadataUpdateCapabilities {
        MetadataUpdateCapabilities::empty()
    }

    /// `sb->s_wb_err` for the single-inode superblock: the very sequence the
    /// root node reports, so `syncfs` and an open description never disagree.
    fn syncfs_writeback_error_state(&self) -> Option<Arc<WritebackErrorState>> {
        self.user_data.writeback_error_state().ok()
    }
}

struct NullDir {
    filesystem: Arc<NullFs>,
    this: crate::WeakDirEntry,
}

impl NodeOps for NullDir {
    fn inode(&self) -> u64 {
        NULLFS_ROOT_INODE
    }

    fn metadata(&self) -> VfsResult<Metadata> {
        let _ = &self.this;
        Ok(Metadata {
            device: 0,
            inode: NULLFS_ROOT_INODE,
            nlink: 2,
            mode: NodePermission::from_bits_truncate(NULLFS_ROOT_MODE),
            node_type: NodeType::Directory,
            uid: 0,
            gid: 0,
            project_id: 0,
            size: 0,
            block_size: 4096,
            blocks: 0,
            rdev: Default::default(),
            atime: Timestamp::ZERO,
            btime: Timestamp::ZERO,
            mtime: Timestamp::ZERO,
            ctime: Timestamp::ZERO,
        })
    }

    fn update_metadata(&self, _update: MetadataUpdate) -> VfsResult<()> {
        Err(immutable())
    }

    fn filesystem(&self) -> &dyn FilesystemOps {
        &*self.filesystem
    }

    fn sync(&self, _data_only: bool) -> VfsResult<()> {
        Ok(())
    }

    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        Some(&NULLFS_FILE_ATTR)
    }

    /// The root is a real inode of a real superblock, so it carries the
    /// superblock's persistent data.  `Location`'s file-attribute and
    /// writeback-error facades require it; Linux's `simple_*` inodes have
    /// always had their `i_private`/errseq equivalents available.
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(&self.filesystem.user_data)
    }

    fn writeback_error_state(&self) -> VfsResult<Arc<WritebackErrorState>> {
        self.filesystem.user_data.writeback_error_state()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

impl DirNodeOps for NullDir {
    fn read_dir(&self, _offset: u64, _sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        Ok(0)
    }

    fn lookup(&self, _name: &FsName) -> VfsResult<DirEntry> {
        Err(VfsError::NotFound)
    }

    fn create_named(
        &self,
        _name: &FsName,
        _options: &NamedCreateOptions,
        _disposition: CreateDisposition,
    ) -> VfsResult<CreateOutcome<DirEntry>> {
        Err(immutable())
    }

    fn link(&self, _name: &FsName, _node: &DirEntry) -> VfsResult<DirEntry> {
        Err(immutable())
    }

    fn unlink(&self, _request: UnlinkRequest<'_>) -> VfsResult<()> {
        Err(immutable())
    }

    fn rename(&self, _request: RenameRequest<'_>) -> VfsResult<()> {
        Err(immutable())
    }
}

/// `nullfs` has no mutable inode attributes; the immutable bit is what
/// `FS_IOC_GETFLAGS`-style consumers and the kernel's own mutation admission
/// test read.
struct NullFileAttr;

impl FileAttrProvider for NullFileAttr {
    fn get_file_attr(&self) -> VfsResult<FileAttr> {
        Ok(FileAttr {
            xflags: FS_XFLAG_IMMUTABLE,
            extsize: 0,
            nextents: 0,
            project_id: 0,
            cowextsize: 0,
        })
    }

    fn set_file_attr(&self, _attr: FileAttr) -> VfsResult<()> {
        Err(immutable())
    }
}

static NULLFS_FILE_ATTR: NullFileAttr = NullFileAttr;
