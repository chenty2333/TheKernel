//! VFS projection of checked Btrfs metadata.  This adapter deliberately owns
//! no pathname strings: Btrfs directory names remain `FsName` bytes through
//! lookup and getdents.

use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    ptr,
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
    task::Context,
};

use axdriver::BlockVolume;
use axerrno::AxError;
use axfs_ng_vfs::{
    DirEntry, DirEntrySink, DirNode, DirNodeOps, FileAttr, FileAttrProvider, FileNode, FileNodeOps,
    FileRangeOperation, FileRangeRequest, Filesystem, FilesystemOps, FsName, LockOps, Metadata,
    MetadataUpdate, NodeFlags, NodeOps, NodeType, NodeUserData, ObjectKey, Reference, StatFs,
    Timestamp, VfsError, VfsResult, WeakDirEntry, XattrProvider, XattrSetMode,
};
use axpoll::{IoEvents, PollRegistration, PollRegistrationError, Pollable};
use axsync::Mutex;
use kspin::SpinNoPreempt as SpinMutex;
use spin::Once;

use super::{BtrfsInodeItem, BtrfsInodeState, BtrfsMount};
use crate::MountedBlockDevice;

const DEFAULT_SUBVOLUME: u64 = 5;
const FS_XFLAG_IMMUTABLE: u64 = 0x0000_0008;
const FS_XFLAG_APPEND: u64 = 0x0000_0010;
const FS_XFLAG_PROJINHERIT: u64 = 0x0000_0200;

/// Native xflag gate evaluated from the same mount/transaction snapshot that
/// the eventual COW planner will commit.  This must precede any planner edit
/// or media-writing helper: immutable/append denial is side-effect free.
fn admit_native_mutation(item: &BtrfsInodeItem, append_at_eof: bool) -> VfsResult<()> {
    if item.flags & FS_XFLAG_IMMUTABLE != 0 || (item.flags & FS_XFLAG_APPEND != 0 && !append_at_eof)
    {
        return Err(VfsError::OperationNotPermitted);
    }
    Ok(())
}

fn native_inode(
    mount: &BtrfsMount,
    root: u64,
    owner: u64,
    inode: u64,
) -> VfsResult<BtrfsInodeItem> {
    mount.inode_item(root, owner, inode).map_err(vfs)
}

pub struct BtrfsFilesystem {
    mount: Mutex<BtrfsMount>,
    _claims: Vec<MountedBlockDevice>,
    tree_owner: u64,
    root_inode: u64,
    device_id: u64,
    inode_state: Mutex<BTreeMap<u64, Weak<BtrfsInodeState>>>,
    /// One entry per native inode generation that has an OFD.  It is kept
    /// separate from the dentry cache: cached aliases must not prolong an
    /// unlinked inode, whereas dup/fork aliases of an OFD share its ticket.
    open_lifecycle: Mutex<BTreeMap<OpenKey, Arc<OpenLifecycle>>>,
    root: SpinMutex<Option<DirEntry>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OpenKey {
    tree_owner: u64,
    inode: u64,
    creation_generation: u64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum OpenPhase {
    Linked,
    PreparingOrphan,
    OrphanDurable,
}
struct OpenLifecycleState {
    live: u64,
    phase: OpenPhase,
}
struct OpenLifecycle {
    state: Mutex<OpenLifecycleState>,
    orphan_work: Mutex<Option<Box<BtrfsOrphanWork>>>,
}

struct OpenTicket {
    fs: Weak<BtrfsFilesystem>,
    key: OpenKey,
    lifecycle: Arc<OpenLifecycle>,
    released: AtomicBool,
}
struct PreparedOrphanClaim {
    lifecycle: Arc<OpenLifecycle>,
    committed: bool,
}
enum LastLinkDisposition {
    DirectNoLive,
    Prepared(PreparedOrphanClaim),
}

struct BtrfsOrphanWork {
    fs: Weak<BtrfsFilesystem>,
    key: OpenKey,
    next: AtomicPtr<BtrfsOrphanWork>,
}

static BTRFS_ORPHAN_WORK: AtomicPtr<BtrfsOrphanWork> = AtomicPtr::new(ptr::null_mut());
static BTRFS_ORPHAN_WAKE: Once<fn()> = Once::new();

fn enqueue_orphan_work(work: Box<BtrfsOrphanWork>) {
    let raw = Box::into_raw(work);
    let mut head = BTRFS_ORPHAN_WORK.load(Ordering::Acquire);
    loop {
        unsafe {
            (*raw).next.store(head, Ordering::Relaxed);
        }
        match BTRFS_ORPHAN_WORK.compare_exchange_weak(
            head,
            raw,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(observed) => head = observed,
        }
    }
    if let Some(wake) = BTRFS_ORPHAN_WAKE.get() {
        wake();
    }
}

pub(super) fn set_deferred_orphan_finalizer_waker(waker: fn()) -> bool {
    let installed = *BTRFS_ORPHAN_WAKE.call_once(|| waker);
    if !BTRFS_ORPHAN_WORK.load(Ordering::Acquire).is_null() {
        installed();
    }
    core::ptr::fn_addr_eq(installed, waker)
}

pub(super) fn has_deferred_orphan_finalizer_work() -> bool {
    !BTRFS_ORPHAN_WORK.load(Ordering::Acquire).is_null()
}

pub(super) fn drain_deferred_orphan_finalizers(mut between: impl FnMut()) -> usize {
    let mut pending = BTRFS_ORPHAN_WORK.swap(ptr::null_mut(), Ordering::Acquire);
    let mut fifo = ptr::null_mut();
    while !pending.is_null() {
        let next = unsafe { (*pending).next.load(Ordering::Relaxed) };
        unsafe {
            (*pending).next.store(fifo, Ordering::Relaxed);
        }
        fifo = pending;
        pending = next;
    }
    let mut completed = 0;
    while !fifo.is_null() {
        let next = unsafe { (*fifo).next.load(Ordering::Relaxed) };
        let work = unsafe { Box::from_raw(fifo) };
        if let Some(fs) = work.fs.upgrade() {
            if fs.retire_orphan_work(work.key).is_ok() {
                completed += 1;
            } else {
                enqueue_orphan_work(work);
            }
        }
        between();
        fifo = next;
    }
    completed
}

impl OpenTicket {
    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(fs) = self.fs.upgrade() else {
            return;
        };
        let mut lifecycles = fs.open_lifecycle.lock();
        let Some(current) = lifecycles.get(&self.key) else {
            return;
        };
        if !Arc::ptr_eq(current, &self.lifecycle) {
            return;
        }
        let mut state = current.state.lock();
        if state.live == 0 {
            return;
        }
        state.live -= 1;
        if state.live != 0 {
            return;
        }
        let phase = state.phase;
        // `current` is borrowed from the registry, so release the per-entry
        // guard before mutating that registry on the final close path.
        drop(state);
        match phase {
            OpenPhase::Linked => {
                lifecycles.remove(&self.key);
            }
            OpenPhase::PreparingOrphan => {} // durable commit owns the work decision.
            OpenPhase::OrphanDurable => {
                drop(lifecycles);
                if let Some(work) = self.lifecycle.orphan_work.lock().take() {
                    enqueue_orphan_work(work);
                }
            }
        }
    }
}

impl Drop for OpenTicket {
    fn drop(&mut self) {
        self.release();
    }
}

impl Drop for PreparedOrphanClaim {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let mut state = self.lifecycle.state.lock();
        if state.phase == OpenPhase::PreparingOrphan {
            state.phase = OpenPhase::Linked;
        }
    }
}

impl BtrfsFilesystem {
    pub fn new(device: MountedBlockDevice) -> VfsResult<Filesystem> {
        Self::new_multi(Vec::from([device]))
    }

    /// Opens a fully supplied Btrfs member set.  Claims are retained for the
    /// filesystem lifetime, so a member cannot be remounted independently
    /// while its chunk/device mapping remains reachable through this volume.
    pub fn new_multi(claims: Vec<MountedBlockDevice>) -> VfsResult<Filesystem> {
        if claims.is_empty() {
            return Err(VfsError::NoSuchDevice);
        }
        let member_is_read_only = claims.iter().any(MountedBlockDevice::is_read_only);
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(claims.len())
            .map_err(|_| VfsError::NoMemory)?;
        for claim in &claims {
            devices.push(claim.device().clone());
        }
        let volume = BlockVolume::new(devices).map_err(|_| VfsError::Io)?;
        let mut mount = if claims.len() == 1 {
            BtrfsMount::open_single(volume)
        } else {
            BtrfsMount::open_multi(volume)
        }
        .map_err(vfs)?;
        if mount.superblock().log_root != 0 {
            // Linux replays a native tree log even for a logical read-only
            // mount.  Only physically read-only members prevent the required
            // recovery transaction; then leave the log untouched and report
            // the real write admission failure.
            if member_is_read_only {
                return Err(VfsError::ReadOnlyFilesystem);
            }
        }
        if member_is_read_only && mount.has_native_orphan_work().map_err(vfs)? {
            return Err(VfsError::ReadOnlyFilesystem);
        }
        // A cleared tree log does not mean there are no durable orphan
        // markers from an interrupted final close.
        mount.replay_inode_log(DEFAULT_SUBVOLUME).map_err(vfs)?;
        let root_inode = mount.superblock().root_dir_objectid;
        let device_id = u64::from_le_bytes(
            mount.superblock().fsid[..8]
                .try_into()
                .map_err(|_| VfsError::Io)?,
        );
        let fs = Arc::try_new(Self {
            mount: Mutex::new(mount),
            _claims: claims,
            tree_owner: DEFAULT_SUBVOLUME,
            root_inode,
            device_id,
            inode_state: Mutex::new(BTreeMap::new()),
            open_lifecycle: Mutex::new(BTreeMap::new()),
            root: SpinMutex::new(None),
        })
        .map_err(|_| VfsError::NoMemory)?;
        let filesystem = Filesystem::try_new(fs.clone())?;
        let root = fs.make_entry(root_inode, NodeType::Directory, Reference::root())?;
        *fs.root.lock() = Some(root);
        Ok(filesystem)
    }

    /// Resolve the current root while holding the mount transaction gate.
    /// A metadata COW commit replaces the ROOT_ITEM bytenr, so retaining the
    /// mount-time value here would subsequently traverse a retired tree.
    fn current_root(mount: &BtrfsMount, tree_owner: u64) -> VfsResult<u64> {
        mount.subvolume_root(tree_owner).map_err(vfs)
    }

    fn inode_item(&self, inode: u64) -> VfsResult<BtrfsInodeItem> {
        let mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        mount.inode_item(root, self.tree_owner, inode).map_err(vfs)
    }

    fn inode_state(&self, inode: u64) -> VfsResult<Arc<BtrfsInodeState>> {
        {
            let mut states = self.inode_state.lock();
            // This registry is weak-only. Prune stale historical inode keys
            // on normal lookup instead of retaining every recycled inode.
            states.retain(|_, state| state.strong_count() != 0);
            if let Some(state) = states.get(&inode).and_then(Weak::upgrade) {
                return Ok(state);
            }
            states.remove(&inode);
        }
        let item = self.inode_item(inode)?;
        let state = Arc::try_new(BtrfsInodeState::new(FileAttr {
            xflags: item.flags,
            extsize: 0,
            nextents: 0,
            project_id: item.project_id,
            cowextsize: 0,
        }))
        .map_err(|_| VfsError::NoMemory)?;
        let mut states = self.inode_state.lock();
        // The inode lookup/allocation runs outside this short registry lock.
        // A concurrent hardlink/relookup may have installed the canonical
        // state while we decoded the item; merge instead of splitting gates.
        if let Some(existing) = states.get(&inode).and_then(Weak::upgrade) {
            return Ok(existing);
        }
        states.insert(inode, Arc::downgrade(&state));
        Ok(state)
    }

    /// Creates one OFD ticket while the native inode generation is sampled
    /// under the mount gate.  This is deliberately not tied to `BtrfsInode`
    /// allocation: dentry aliases and hard links all rendezvous here.
    fn acquire_open_ticket(self: &Arc<Self>, inode: u64) -> VfsResult<Arc<OpenTicket>> {
        let mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let item = mount
            .inode_item(root, self.tree_owner, inode)
            .map_err(vfs)?;
        let key = OpenKey {
            tree_owner: self.tree_owner,
            inode,
            creation_generation: item.generation,
        };
        let lifecycle = {
            let mut lifecycles = self.open_lifecycle.lock();
            if let Some(existing) = lifecycles.get(&key) {
                existing.clone()
            } else {
                let work = Box::try_new(BtrfsOrphanWork {
                    fs: Arc::downgrade(self),
                    key,
                    next: AtomicPtr::new(ptr::null_mut()),
                })
                .map_err(|_| VfsError::NoMemory)?;
                let state = Arc::try_new(OpenLifecycle {
                    state: Mutex::new(OpenLifecycleState {
                        live: 0,
                        phase: OpenPhase::Linked,
                    }),
                    orphan_work: Mutex::new(Some(work)),
                })
                .map_err(|_| VfsError::NoMemory)?;
                lifecycles.insert(key, state.clone());
                state
            }
        };
        {
            let mut state = lifecycle.state.lock();
            state.live = state.live.checked_add(1).ok_or(VfsError::StorageFull)?;
        }
        match Arc::try_new(OpenTicket {
            fs: Arc::downgrade(self),
            key,
            lifecycle,
            released: AtomicBool::new(false),
        }) {
            Ok(ticket) => Ok(ticket),
            Err(_) => {
                // `work` is dropped with the failed allocation; undo the
                // admission so a failed open cannot pin a later orphan.
                if let Some(current) = self.open_lifecycle.lock().get(&key) {
                    let mut state = current.state.lock();
                    state.live = state.live.saturating_sub(1);
                }
                Err(VfsError::NoMemory)
            }
        }
    }

    /// Claims an already-open native generation for deferred orphaning while
    /// holding the one lifecycle registry lock.  Final close uses that same
    /// lock before it can remove an idle entry, so a last-link decision never
    /// races a zero-count observation into an immediate retirement.
    fn claim_open_orphan(&self, key: OpenKey) -> Option<PreparedOrphanClaim> {
        let lifecycles = self.open_lifecycle.lock();
        let lifecycle = lifecycles.get(&key)?.clone();
        let mut state = lifecycle.state.lock();
        if state.live == 0 || state.phase != OpenPhase::Linked {
            return None;
        }
        state.phase = OpenPhase::PreparingOrphan;
        // The claim deliberately outlives both guards.  In particular, do not
        // move `lifecycle` into it while its state guard is still live: that
        // would retain a borrow into the lifecycle across the deferred COW
        // commit and is rejected by the no_std borrow checker.
        drop(state);
        drop(lifecycles);
        Some(PreparedOrphanClaim {
            lifecycle,
            committed: false,
        })
    }

    /// Samples and claims the final-link lifecycle in one registry critical
    /// section.  Namespace planning must use this disposition verbatim;
    /// resampling after it has removed the name would race final close.
    fn prepare_last_link(&self, key: OpenKey) -> LastLinkDisposition {
        match self.claim_open_orphan(key) {
            Some(claim) => LastLinkDisposition::Prepared(claim),
            None => LastLinkDisposition::DirectNoLive,
        }
    }

    fn commit_open_orphan(&self, claim: &mut PreparedOrphanClaim) {
        let mut queue = None;
        {
            let _lifecycles = self.open_lifecycle.lock();
            let mut state = claim.lifecycle.state.lock();
            if state.phase != OpenPhase::PreparingOrphan {
                return;
            }
            state.phase = OpenPhase::OrphanDurable;
            if state.live == 0 {
                queue = claim.lifecycle.orphan_work.lock().take();
            }
        }
        if let Some(work) = queue {
            enqueue_orphan_work(work);
        }
        claim.committed = true;
    }

    fn retire_orphan_work(&self, key: OpenKey) -> VfsResult<()> {
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, key.tree_owner)?;
        let marker = super::OrphanRetirement::new(key.tree_owner, key.inode)
            .map_err(vfs)?
            .marker_key();
        let inode = match mount.inode_item(root, key.tree_owner, key.inode) {
            Ok(inode) => inode,
            Err(AxError::NotFound) => return Ok(()),
            Err(error) => return Err(vfs(error)),
        };
        if inode.generation != key.creation_generation || inode.nlink != 0 {
            return Ok(());
        }
        let mut planner = mount.mutation_planner(key.tree_owner).map_err(vfs)?;
        if planner
            .tree_items(key.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&marker, |item| item.key)
            .is_err()
        {
            return Ok(());
        }
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let free_space = mount.logical_allocator().map_err(vfs)?;
        let freed = retire_last_link_inode(
            &mount,
            &mut planner,
            key.tree_owner,
            key.inode,
            generation,
            &free_space,
        )?;
        planner
            .finish_logged_extent_accounting(&free_space)
            .map_err(vfs)?;
        let bytes_used = mount
            .superblock()
            .bytes_used
            .checked_sub(freed)
            .ok_or(VfsError::Io)?;
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        self.open_lifecycle.lock().remove(&key);
        Ok(())
    }

    fn persist_file_attr(&self, inode: u64, attr: FileAttr) -> VfsResult<()> {
        let mut mount = self.mount.lock();
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let key = super::TreeItemKey {
            objectid: inode,
            item_type: super::INODE_ITEM,
            offset: 0,
        };
        let old = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&key, |item| item.key)
            .map_err(|_| VfsError::NotFound)?;
        let mut item =
            BtrfsInodeItem::decode(&planner.tree_items(self.tree_owner).map_err(vfs)?[old].value)
                .map_err(vfs)?;
        item.flags = attr.xflags;
        item.project_id = attr.project_id;
        planner
            .set_item(self.tree_owner, key, item.encode())
            .map_err(vfs)?;
        let bytes_used = mount.superblock().bytes_used;
        // `commit_mutation_planner` is the one durability boundary: it
        // publishes the COW root/superblock only after flushing the volume.
        // Do not add a second provider flush after publication.
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        // Publish while retaining the mount gate so a later fileattr commit
        // cannot be overwritten by this older cache update.  Inode creation
        // never holds the state lock while acquiring the mount gate.
        if let Some(state) = self.inode_state.lock().get(&inode).and_then(Weak::upgrade) {
            state.set_file_attr(attr)?;
            state.mark_file_attr_persisted();
        }
        Ok(())
    }

    fn persist_metadata(&self, inode: u64, update: MetadataUpdate) -> VfsResult<()> {
        let mut mount = self.mount.lock();
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let key = super::TreeItemKey {
            objectid: inode,
            item_type: super::INODE_ITEM,
            offset: 0,
        };
        let index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&key, |item| item.key)
            .map_err(|_| VfsError::NotFound)?;
        let mut item =
            BtrfsInodeItem::decode(&planner.tree_items(self.tree_owner).map_err(vfs)?[index].value)
                .map_err(vfs)?;
        admit_native_mutation(&item, false)?;
        if let Some(mode) = update.mode {
            item.mode = (item.mode & !0o7777) | u32::from(mode.bits());
        }
        if let Some((uid, gid)) = update.owner {
            item.uid = uid;
            item.gid = gid;
        }
        if let Some(project_id) = update.project_id {
            item.project_id = project_id;
        }
        if let Some(rdev) = update.rdev {
            item.rdev = rdev.0;
        }
        if let Some(atime) = update.atime {
            item.atime = atime;
        }
        if let Some(mtime) = update.mtime {
            item.mtime = mtime;
        }
        if let Some(ctime) = update.ctime {
            item.ctime = ctime;
        }
        let published_project_id = item.project_id;
        planner
            .set_item(self.tree_owner, key, item.encode())
            .map_err(vfs)?;
        let bytes_used = mount.superblock().bytes_used;
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        // Update the visible cache only after publication, while preserving
        // the mount-commit order against a concurrent fileattr update.
        if let Some(state) = self.inode_state.lock().get(&inode).and_then(Weak::upgrade) {
            let mut attr = state.get_file_attr()?;
            attr.project_id = published_project_id;
            state.set_file_attr(attr)?;
            state.mark_file_attr_persisted();
        }
        Ok(())
    }

    fn persist_xattr(
        &self,
        inode: u64,
        name: &[u8],
        value: Option<&[u8]>,
        mode: XattrSetMode,
    ) -> VfsResult<()> {
        if name.is_empty() || name.len() > 255 {
            return Err(VfsError::InvalidInput);
        }
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let inode_item = native_inode(&mount, root, self.tree_owner, inode)?;
        admit_native_mutation(&inode_item, false)?;
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let key = super::TreeItemKey {
            objectid: inode,
            item_type: super::XATTR_ITEM,
            offset: u64::from(super::crc32c(name)),
        };
        let previous = {
            let items = planner.tree_items(self.tree_owner).map_err(vfs)?;
            match items.binary_search_by_key(&key, |item| item.key) {
                Ok(index) => super::decode_dir_items(&items[index].value).map_err(vfs)?,
                Err(_) => Vec::new(),
            }
        };
        let mut entries = previous;
        let position = entries.iter().position(|entry| entry.name == name);
        match value {
            Some(value) => {
                match (mode, position) {
                    (XattrSetMode::Create, Some(_)) | (XattrSetMode::CreateAndReplace, Some(_)) => {
                        return Err(VfsError::AlreadyExists);
                    }
                    (XattrSetMode::Replace, None) | (XattrSetMode::CreateAndReplace, None) => {
                        return Err(VfsError::NotFound);
                    }
                    _ => {}
                }
                let mut name_owned = Vec::new();
                name_owned
                    .try_reserve_exact(name.len())
                    .map_err(|_| VfsError::NoMemory)?;
                name_owned.extend_from_slice(name);
                let mut value_owned = Vec::new();
                value_owned
                    .try_reserve_exact(value.len())
                    .map_err(|_| VfsError::NoMemory)?;
                value_owned.extend_from_slice(value);
                let entry = super::BtrfsDirItem {
                    inode,
                    location_type: 0,
                    location_offset: 0,
                    item_type: 0,
                    transid: mount.superblock().generation.saturating_add(1),
                    name: name_owned,
                    data: value_owned,
                };
                if let Some(position) = position {
                    entries[position] = entry;
                } else {
                    entries.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    entries.push(entry);
                }
            }
            None => {
                let position = position.ok_or(VfsError::NotFound)?;
                entries.remove(position);
            }
        }
        if entries.is_empty() {
            let _ = planner.delete_item(self.tree_owner, key).map_err(vfs)?;
        } else {
            planner
                .set_item(
                    self.tree_owner,
                    key,
                    super::encode_dir_items(&entries).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        let bytes_used = mount.superblock().bytes_used;
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        Ok(())
    }

    /// Publishes a new inode and, for a symlink, its native inline target in
    /// the same COW transaction.  In particular, a lookup can never observe
    /// a newly-created empty link while a second operation fills its target.
    fn create_inode(
        &self,
        parent: u64,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
        symlink_target: Option<&axfs_ng_vfs::FsPath>,
    ) -> VfsResult<(u64, bool)> {
        if name.as_bytes().is_empty() || name.as_bytes() == b"." || name.as_bytes() == b".." {
            return Err(VfsError::InvalidInput);
        }
        // The Btrfs transaction below owns inode and directory-item COW as a
        // single commit.  Do not publish first and add ACL xattrs later: this
        // provider has not yet gained native ACL item encoding, so reject a
        // requested payload before touching the transaction.
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        if let Ok(existing) = mount.lookup_dir_item(root, self.tree_owner, parent, name.as_bytes())
        {
            return Ok((existing.inode, false));
        }
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let fs_items = planner.tree_items(self.tree_owner).map_err(vfs)?;
        let inode = fs_items
            .iter()
            .filter(|item| item.key.item_type == super::INODE_ITEM)
            .map(|item| item.key.objectid)
            .max()
            .unwrap_or(256)
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let parent_item = BtrfsInodeItem::decode(
            &fs_items[fs_items
                .binary_search_by_key(
                    &super::TreeItemKey {
                        objectid: parent,
                        item_type: super::INODE_ITEM,
                        offset: 0,
                    },
                    |item| item.key,
                )
                .map_err(|_| VfsError::NotFound)?]
            .value,
        )
        .map_err(vfs)?;
        admit_native_mutation(&parent_item, false)?;
        let index = fs_items
            .iter()
            .filter(|item| item.key.objectid == parent && item.key.item_type == super::DIR_INDEX)
            .map(|item| item.key.offset)
            .max()
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let (uid, gid) = options.owner.unwrap_or((parent_item.uid, parent_item.gid));
        let rdev = options.rdev.map_or(0, |device| device.0);
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        if symlink_target.is_some() != (options.node_type == NodeType::Symlink) {
            return Err(VfsError::InvalidInput);
        }
        let project_id = if parent_item.flags & FS_XFLAG_PROJINHERIT != 0 {
            parent_item.project_id
        } else {
            options.initial_attributes.project_id.unwrap_or(0)
        };
        let mut item = BtrfsInodeItem::new(
            generation,
            options.node_type,
            options.permission,
            uid,
            gid,
            rdev,
            project_id,
            Timestamp::ZERO,
        )
        .map_err(vfs)?;
        // `BtrfsInodeItem::flags` is the provider's native persisted fileattr
        // word (also used by `persist_file_attr`).  Carry PROJINHERIT into the
        // same inode item which is committed with the new directory entry.
        if options.initial_attributes.project_inherit
            || parent_item.flags & FS_XFLAG_PROJINHERIT != 0
        {
            item.flags |= FS_XFLAG_PROJINHERIT;
        }
        if let Some(target) = symlink_target {
            if target.as_bytes().is_empty() {
                return Err(VfsError::InvalidInput);
            }
            let target_len =
                u64::try_from(target.as_bytes().len()).map_err(|_| VfsError::StorageFull)?;
            let inline_limit = u64::from(mount.superblock().nodesize).saturating_sub(512);
            // Linux Btrfs accepts long symlinks; the regular extent path is
            // used when the native leaf cannot contain the target.  The
            // target still becomes reachable only with its DIR_ITEM.
            if target_len > inline_limit {
                return Err(VfsError::StorageFull);
            }
            item.size = target_len;
            item.nbytes = target_len;
            planner
                .set_item(
                    self.tree_owner,
                    super::TreeItemKey {
                        objectid: inode,
                        item_type: super::EXTENT_DATA,
                        offset: 0,
                    },
                    super::encode_inline_extent(generation, target.as_bytes()).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: inode,
                    item_type: super::INODE_ITEM,
                    offset: 0,
                },
                item.encode(),
            )
            .map_err(vfs)?;
        // XATTR_ITEM uses the same collision-bucket encoding as directory
        // items. Install POSIX ACL records into this very planner before its
        // one COW commit, so lookup cannot observe the new name first.
        for (xattr_name, value) in [
            (
                b"system.posix_acl_access".as_slice(),
                options.initial_attributes.access_acl.as_deref(),
            ),
            (
                b"system.posix_acl_default".as_slice(),
                options.initial_attributes.default_acl.as_deref(),
            ),
        ] {
            let Some(value) = value else { continue };
            if xattr_name.len() > 255 || value.len() > u16::MAX as usize {
                return Err(VfsError::InvalidInput);
            }
            let key = super::TreeItemKey {
                objectid: inode,
                item_type: super::XATTR_ITEM,
                offset: u64::from(super::crc32c(xattr_name)),
            };
            let entry = super::BtrfsDirItem {
                inode,
                location_type: 0,
                location_offset: 0,
                item_type: 0,
                transid: generation,
                name: Vec::from(xattr_name),
                data: Vec::from(value),
            };
            planner
                .set_item(
                    self.tree_owner,
                    key,
                    super::encode_dir_items(&[entry]).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        let dir_entry = super::BtrfsDirItem {
            inode,
            location_type: super::INODE_ITEM,
            location_offset: 0,
            item_type: dir_type(options.node_type),
            transid: generation,
            name: Vec::from(name.as_bytes()),
            data: Vec::new(),
        };
        let dir_key = super::TreeItemKey {
            objectid: parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(name.as_bytes())),
        };
        let mut bucket = {
            let items = planner.tree_items(self.tree_owner).map_err(vfs)?;
            match items.binary_search_by_key(&dir_key, |item| item.key) {
                Ok(index) => super::decode_dir_items(&items[index].value).map_err(vfs)?,
                Err(_) => Vec::new(),
            }
        };
        bucket.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        bucket.push(dir_entry.clone());
        planner
            .set_item(
                self.tree_owner,
                dir_key,
                super::encode_dir_items(&bucket).map_err(vfs)?,
            )
            .map_err(vfs)?;
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: parent,
                    item_type: super::DIR_INDEX,
                    offset: index,
                },
                super::encode_dir_items(&[dir_entry]).map_err(vfs)?,
            )
            .map_err(vfs)?;
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: inode,
                    item_type: super::INODE_REF,
                    offset: parent,
                },
                super::encode_inode_refs(&[super::BtrfsInodeRef {
                    index,
                    name: Vec::from(name.as_bytes()),
                }])
                .map_err(vfs)?,
            )
            .map_err(vfs)?;
        if options.node_type == NodeType::Directory {
            adjust_parent_directory_nlink(&mut planner, self.tree_owner, parent, 1, generation)?;
        }
        let bytes_used = mount.superblock().bytes_used;
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        Ok((inode, true))
    }

    fn link_inode(&self, parent: u64, name: &FsName, target: &BtrfsInode) -> VfsResult<()> {
        if name.as_bytes().is_empty() || name.as_bytes() == b"." || name.as_bytes() == b".." {
            return Err(VfsError::InvalidInput);
        }
        if target.object_key().filesystem != self.device_id {
            return Err(VfsError::CrossesDevices);
        }
        let kind = target
            .item()?
            .metadata(target.inode, self.device_id, 0)
            .map_err(vfs)?
            .node_type;
        if kind == NodeType::Directory {
            return Err(VfsError::OperationNotPermitted);
        }
        if kind == NodeType::Unknown {
            return Err(VfsError::InvalidData);
        }
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let parent_item = native_inode(&mount, root, self.tree_owner, parent)?;
        let target_item = native_inode(&mount, root, self.tree_owner, target.inode)?;
        admit_native_mutation(&parent_item, false)?;
        admit_native_mutation(&target_item, false)?;
        if mount
            .lookup_dir_item(root, self.tree_owner, parent, name.as_bytes())
            .is_ok()
        {
            return Err(VfsError::AlreadyExists);
        }
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let bytes_used = mount.superblock().bytes_used;
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let ref_key = super::TreeItemKey {
            objectid: target.inode,
            item_type: super::INODE_REF,
            offset: parent,
        };
        let inode_key = super::TreeItemKey {
            objectid: target.inode,
            item_type: super::INODE_ITEM,
            offset: 0,
        };
        let inode_index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&inode_key, |item| item.key)
            .map_err(|_| VfsError::NotFound)?;
        let mut inode = BtrfsInodeItem::decode(
            &planner.tree_items(self.tree_owner).map_err(vfs)?[inode_index].value,
        )
        .map_err(vfs)?;
        inode.nlink = inode.nlink.checked_add(1).ok_or(VfsError::StorageFull)?;
        inode.transid = generation;
        planner
            .set_item(self.tree_owner, inode_key, inode.encode())
            .map_err(vfs)?;
        let index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .iter()
            .filter(|item| item.key.objectid == parent && item.key.item_type == super::DIR_INDEX)
            .map(|item| item.key.offset)
            .max()
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let entry = super::BtrfsDirItem {
            inode: target.inode,
            location_type: super::INODE_ITEM,
            location_offset: 0,
            item_type: dir_type(kind),
            transid: generation,
            name: Vec::from(name.as_bytes()),
            data: Vec::new(),
        };
        let dir_key = super::TreeItemKey {
            objectid: parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(name.as_bytes())),
        };
        let mut bucket = match planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&dir_key, |item| item.key)
        {
            Ok(index) => super::decode_dir_items(
                &planner.tree_items(self.tree_owner).map_err(vfs)?[index].value,
            )
            .map_err(vfs)?,
            Err(_) => Vec::new(),
        };
        bucket.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        bucket.push(entry.clone());
        planner
            .set_item(
                self.tree_owner,
                dir_key,
                super::encode_dir_items(&bucket).map_err(vfs)?,
            )
            .map_err(vfs)?;
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: parent,
                    item_type: super::DIR_INDEX,
                    offset: index,
                },
                super::encode_dir_items(&[entry]).map_err(vfs)?,
            )
            .map_err(vfs)?;
        let mut ordinary = match planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&ref_key, |item| item.key)
        {
            Ok(position) => super::decode_inode_refs(
                &planner.tree_items(self.tree_owner).map_err(vfs)?[position].value,
            )
            .map_err(vfs)?,
            Err(_) => Vec::new(),
        };
        if ordinary.iter().any(|reference| {
            reference.index == index && reference.name.as_slice() == name.as_bytes()
        }) {
            return Err(VfsError::AlreadyExists);
        }
        ordinary.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
        ordinary.push(super::BtrfsInodeRef {
            index,
            name: Vec::from(name.as_bytes()),
        });
        // Leaf splitting is owned by the COW writer; an ordinary ref bucket
        // remains native until it alone exceeds a legal one-item leaf.
        if super::encode_inode_refs(&ordinary).map_err(vfs)?.len()
            <= (mount.superblock().nodesize as usize).saturating_sub(0x65 + 25)
        {
            planner
                .set_item(
                    self.tree_owner,
                    ref_key,
                    super::encode_inode_refs(&ordinary).map_err(vfs)?,
                )
                .map_err(vfs)?;
        } else {
            // Native EXTREF fallback, including collision packing.
            let ext_key = super::TreeItemKey {
                objectid: target.inode,
                item_type: super::INODE_EXTREF,
                offset: super::btrfs_extref_hash(parent, name.as_bytes()),
            };
            let mut extrefs = match planner
                .tree_items(self.tree_owner)
                .map_err(vfs)?
                .binary_search_by_key(&ext_key, |item| item.key)
            {
                Ok(position) => super::decode_inode_extrefs(
                    &planner.tree_items(self.tree_owner).map_err(vfs)?[position].value,
                )
                .map_err(vfs)?,
                Err(_) => Vec::new(),
            };
            if extrefs.iter().any(|(existing_parent, _, existing_name)| {
                *existing_parent == parent && existing_name.as_slice() == name.as_bytes()
            }) {
                return Err(VfsError::AlreadyExists);
            }
            extrefs.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
            extrefs.push((parent, index, Vec::from(name.as_bytes())));
            planner
                .set_item(
                    self.tree_owner,
                    ext_key,
                    super::encode_inode_extrefs(&extrefs).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        Ok(())
    }

    fn unlink_name(
        &self,
        parent: u64,
        name: &FsName,
        expected: Option<u64>,
        is_dir: bool,
    ) -> VfsResult<()> {
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let live = mount
            .lookup_dir_item(root, self.tree_owner, parent, name.as_bytes())
            .map_err(vfs)?;
        let parent_item = native_inode(&mount, root, self.tree_owner, parent)?;
        let target_item = native_inode(&mount, root, self.tree_owner, live.inode)?;
        admit_native_mutation(&parent_item, false)?;
        admit_native_mutation(&target_item, false)?;
        if expected.is_some_and(|inode| inode != live.inode) {
            return Err(VfsError::ResourceBusy);
        }
        if is_dir != (kind_from_dir_type(live.item_type) == NodeType::Directory) {
            return Err(VfsError::InvalidInput);
        }
        if is_dir
            && !mount
                .directory_items(root, self.tree_owner, live.inode)
                .map_err(vfs)?
                .is_empty()
        {
            return Err(VfsError::DirectoryNotEmpty);
        }
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let dir_key = super::TreeItemKey {
            objectid: parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(name.as_bytes())),
        };
        let mut bucket = {
            let items = planner.tree_items(self.tree_owner).map_err(vfs)?;
            let index = items
                .binary_search_by_key(&dir_key, |item| item.key)
                .map_err(|_| VfsError::NotFound)?;
            super::decode_dir_items(&items[index].value).map_err(vfs)?
        };
        bucket.retain(|item| item.name != name.as_bytes());
        if bucket.is_empty() {
            let _ = planner.delete_item(self.tree_owner, dir_key).map_err(vfs)?;
        } else {
            planner
                .set_item(
                    self.tree_owner,
                    dir_key,
                    super::encode_dir_items(&bucket).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        let index_key = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .iter()
            .find(|item| {
                item.key.objectid == parent
                    && item.key.item_type == super::DIR_INDEX
                    && super::decode_dir_items(&item.value)
                        .ok()
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| entry.name == name.as_bytes())
                        })
            })
            .map(|item| item.key)
            .ok_or(VfsError::NotFound)?;
        let _ = planner
            .delete_item(self.tree_owner, index_key)
            .map_err(vfs)?;
        remove_inode_backref(
            &mut planner,
            self.tree_owner,
            live.inode,
            parent,
            name.as_bytes(),
        )?;
        let lifecycle_key = OpenKey {
            tree_owner: self.tree_owner,
            inode: live.inode,
            creation_generation: target_item.generation,
        };
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let mut freed = 0u64;
        let mut prepared =
            if (is_dir && target_item.nlink == 2) || (!is_dir && target_item.nlink == 1) {
                match self.prepare_last_link(lifecycle_key) {
                    LastLinkDisposition::DirectNoLive => None,
                    LastLinkDisposition::Prepared(claim) => Some(claim),
                }
            } else {
                None
            };
        if is_dir {
            if target_item.nlink != 2 {
                return Err(VfsError::Io);
            }
            if prepared.is_none() {
                retire_empty_directory_links(&mut planner, self.tree_owner, live.inode)?;
            } else {
                let inode_key = super::TreeItemKey {
                    objectid: live.inode,
                    item_type: super::INODE_ITEM,
                    offset: 0,
                };
                let mut inode = BtrfsInodeItem::decode(
                    &planner.tree_items(self.tree_owner).map_err(vfs)?[planner
                        .tree_items(self.tree_owner)
                        .map_err(vfs)?
                        .binary_search_by_key(&inode_key, |item| item.key)
                        .map_err(|_| VfsError::NotFound)?]
                    .value,
                )
                .map_err(vfs)?;
                inode.nlink = 0;
                inode.transid = generation;
                planner
                    .set_item(self.tree_owner, inode_key, inode.encode())
                    .map_err(vfs)?;
                planner
                    .set_item(
                        self.tree_owner,
                        super::OrphanRetirement::new(self.tree_owner, live.inode)
                            .map_err(vfs)?
                            .marker_key(),
                        Vec::new(),
                    )
                    .map_err(vfs)?;
            }
        } else {
            let inode_key = super::TreeItemKey {
                objectid: live.inode,
                item_type: super::INODE_ITEM,
                offset: 0,
            };
            let inode_index = planner
                .tree_items(self.tree_owner)
                .map_err(vfs)?
                .binary_search_by_key(&inode_key, |item| item.key)
                .map_err(|_| VfsError::NotFound)?;
            let mut inode = BtrfsInodeItem::decode(
                &planner.tree_items(self.tree_owner).map_err(vfs)?[inode_index].value,
            )
            .map_err(vfs)?;
            inode.nlink = inode.nlink.checked_sub(1).ok_or(VfsError::Io)?;
            if inode.nlink == 0 && prepared.is_none() {
                let free_space = mount.logical_allocator().map_err(vfs)?;
                freed = retire_last_link_inode(
                    &mount,
                    &mut planner,
                    self.tree_owner,
                    live.inode,
                    generation,
                    &free_space,
                )?;
                planner
                    .finish_logged_extent_accounting(&free_space)
                    .map_err(vfs)?;
            } else {
                if inode.nlink == 0 {
                    inode.transid = generation;
                    planner
                        .set_item(self.tree_owner, inode_key, inode.encode())
                        .map_err(vfs)?;
                    planner
                        .set_item(
                            self.tree_owner,
                            super::OrphanRetirement::new(self.tree_owner, live.inode)
                                .map_err(vfs)?
                                .marker_key(),
                            Vec::new(),
                        )
                        .map_err(vfs)?;
                } else {
                    planner
                        .set_item(self.tree_owner, inode_key, inode.encode())
                        .map_err(vfs)?;
                }
            }
        }
        if is_dir {
            adjust_parent_directory_nlink(&mut planner, self.tree_owner, parent, -1, generation)?;
        }
        let bytes_used = mount
            .superblock()
            .bytes_used
            .checked_sub(freed)
            .ok_or(VfsError::Io)?;
        let commit = mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs);
        if commit.is_ok() {
            if let Some(claim) = prepared.as_mut() {
                self.commit_open_orphan(claim);
            }
        }
        commit?;
        Ok(())
    }

    /// Renames one native directory record under the one mount transaction
    /// gate.  This deliberately edits DIR_ITEM, DIR_INDEX and INODE_REF in
    /// the same COW image; a lookup-visible name is never published without
    /// its index/reference counterpart.
    fn rename_name(
        &self,
        source_parent: u64,
        source_name: &FsName,
        source_inode: u64,
        destination_parent: u64,
        destination_name: &FsName,
        expected_destination: Option<u64>,
    ) -> VfsResult<()> {
        if source_name.as_bytes().is_empty()
            || destination_name.as_bytes().is_empty()
            || source_name.as_bytes() == b"."
            || source_name.as_bytes() == b".."
            || destination_name.as_bytes() == b"."
            || destination_name.as_bytes() == b".."
        {
            return Err(VfsError::InvalidInput);
        }
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let source = mount
            .lookup_dir_item(root, self.tree_owner, source_parent, source_name.as_bytes())
            .map_err(vfs)?;
        if source.inode != source_inode {
            return Err(VfsError::ResourceBusy);
        }
        let source_kind = kind_from_dir_type(source.item_type);
        let destination = match mount.lookup_dir_item(
            root,
            self.tree_owner,
            destination_parent,
            destination_name.as_bytes(),
        ) {
            Ok(entry) => Some(entry),
            Err(AxError::NotFound) => None,
            Err(error) => return Err(vfs(error)),
        };
        match (expected_destination, destination.as_ref()) {
            (None, None) => {}
            (Some(expected), Some(actual)) if expected == actual.inode => {}
            _ => return Err(VfsError::ResourceBusy),
        }
        let source_parent_item = native_inode(&mount, root, self.tree_owner, source_parent)?;
        let destination_parent_item =
            native_inode(&mount, root, self.tree_owner, destination_parent)?;
        let source_inode_item = native_inode(&mount, root, self.tree_owner, source.inode)?;
        admit_native_mutation(&source_parent_item, false)?;
        admit_native_mutation(&destination_parent_item, false)?;
        admit_native_mutation(&source_inode_item, false)?;
        if let Some(destination) = destination.as_ref() {
            let destination_inode_item =
                native_inode(&mount, root, self.tree_owner, destination.inode)?;
            admit_native_mutation(&destination_inode_item, false)?;
        }
        if destination
            .as_ref()
            .is_some_and(|entry| entry.inode == source_inode)
        {
            return Ok(());
        }
        if destination
            .as_ref()
            .is_some_and(|entry| kind_from_dir_type(entry.item_type) != source_kind)
        {
            return Err(VfsError::InvalidInput);
        }
        let destination_is_directory = destination
            .as_ref()
            .is_some_and(|entry| kind_from_dir_type(entry.item_type) == NodeType::Directory);
        let destination_last_link = if destination_is_directory {
            false
        } else {
            match destination.as_ref() {
                Some(entry) => native_inode(&mount, root, self.tree_owner, entry.inode)?.nlink == 1,
                None => false,
            }
        };
        if source_kind == NodeType::Directory {
            reject_directory_rename_cycle(
                source_inode,
                destination_parent,
                self.root_inode,
                |inode| {
                    mount
                        .directory_parent(root, self.tree_owner, inode, self.root_inode)
                        .map_err(vfs)
                },
            )?;
            if let Some(entry) = destination.as_ref() {
                if !mount
                    .directory_items(root, self.tree_owner, entry.inode)
                    .map_err(vfs)?
                    .is_empty()
                {
                    return Err(VfsError::DirectoryNotEmpty);
                }
            }
        }
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let bytes_used = mount.superblock().bytes_used;
        let mut destination_prepared = match destination.as_ref() {
            Some(entry) => {
                let item = native_inode(&mount, root, self.tree_owner, entry.inode)?;
                let last = if destination_is_directory {
                    item.nlink == 2
                } else {
                    item.nlink == 1
                };
                if !last {
                    None
                } else {
                    match self.prepare_last_link(OpenKey {
                        tree_owner: self.tree_owner,
                        inode: entry.inode,
                        creation_generation: item.generation,
                    }) {
                        LastLinkDisposition::DirectNoLive => None,
                        LastLinkDisposition::Prepared(claim) => Some(claim),
                    }
                }
            }
            None => None,
        };
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let mut freed = 0u64;
        let mut destination_orphan: Option<OpenKey> = None;

        let source_dir_key = super::TreeItemKey {
            objectid: source_parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(source_name.as_bytes())),
        };
        let mut source_bucket = load_dir_bucket(&planner, self.tree_owner, source_dir_key)?;
        source_bucket.retain(|entry| entry.name != source_name.as_bytes());
        store_dir_bucket(&mut planner, self.tree_owner, source_dir_key, source_bucket)?;
        let source_index_key = find_dir_index(
            &planner,
            self.tree_owner,
            source_parent,
            source_name.as_bytes(),
        )?;
        let _ = planner
            .delete_item(self.tree_owner, source_index_key)
            .map_err(vfs)?;

        if let Some(destination) = destination {
            let destination_dir_key = super::TreeItemKey {
                objectid: destination_parent,
                item_type: super::DIR_ITEM,
                offset: u64::from(super::crc32c(destination_name.as_bytes())),
            };
            let mut destination_bucket =
                load_dir_bucket(&planner, self.tree_owner, destination_dir_key)?;
            destination_bucket.retain(|entry| entry.name != destination_name.as_bytes());
            store_dir_bucket(
                &mut planner,
                self.tree_owner,
                destination_dir_key,
                destination_bucket,
            )?;
            let destination_index_key = find_dir_index(
                &planner,
                self.tree_owner,
                destination_parent,
                destination_name.as_bytes(),
            )?;
            let _ = planner
                .delete_item(self.tree_owner, destination_index_key)
                .map_err(vfs)?;
            remove_inode_backref(
                &mut planner,
                self.tree_owner,
                destination.inode,
                destination_parent,
                destination_name.as_bytes(),
            )?;
            let destination_item = native_inode(&mount, root, self.tree_owner, destination.inode)?;
            let destination_key = OpenKey {
                tree_owner: self.tree_owner,
                inode: destination.inode,
                creation_generation: destination_item.generation,
            };
            let destination_live = destination_prepared.is_some();
            if destination_is_directory && !destination_live {
                retire_empty_directory_links(&mut planner, self.tree_owner, destination.inode)?;
            } else if destination_is_directory {
                let inode_key = super::TreeItemKey {
                    objectid: destination.inode,
                    item_type: super::INODE_ITEM,
                    offset: 0,
                };
                let index = planner
                    .tree_items(self.tree_owner)
                    .map_err(vfs)?
                    .binary_search_by_key(&inode_key, |item| item.key)
                    .map_err(|_| VfsError::NotFound)?;
                let mut inode = BtrfsInodeItem::decode(
                    &planner.tree_items(self.tree_owner).map_err(vfs)?[index].value,
                )
                .map_err(vfs)?;
                inode.nlink = 0;
                inode.transid = generation;
                planner
                    .set_item(self.tree_owner, inode_key, inode.encode())
                    .map_err(vfs)?;
                planner
                    .set_item(
                        self.tree_owner,
                        super::OrphanRetirement::new(self.tree_owner, destination.inode)
                            .map_err(vfs)?
                            .marker_key(),
                        Vec::new(),
                    )
                    .map_err(vfs)?;
                destination_orphan = Some(destination_key);
            } else if destination_last_link {
                if !destination_live {
                    let free_space = mount.logical_allocator().map_err(vfs)?;
                    let released = retire_last_link_inode(
                        &mount,
                        &mut planner,
                        self.tree_owner,
                        destination.inode,
                        generation,
                        &free_space,
                    )?;
                    freed = freed.checked_add(released).ok_or(VfsError::StorageFull)?;
                    planner
                        .finish_logged_extent_accounting(&free_space)
                        .map_err(vfs)?;
                } else {
                    let inode_key = super::TreeItemKey {
                        objectid: destination.inode,
                        item_type: super::INODE_ITEM,
                        offset: 0,
                    };
                    let index = planner
                        .tree_items(self.tree_owner)
                        .map_err(vfs)?
                        .binary_search_by_key(&inode_key, |item| item.key)
                        .map_err(|_| VfsError::NotFound)?;
                    let mut inode = BtrfsInodeItem::decode(
                        &planner.tree_items(self.tree_owner).map_err(vfs)?[index].value,
                    )
                    .map_err(vfs)?;
                    inode.nlink = 0;
                    inode.transid = generation;
                    planner
                        .set_item(self.tree_owner, inode_key, inode.encode())
                        .map_err(vfs)?;
                    planner
                        .set_item(
                            self.tree_owner,
                            super::OrphanRetirement::new(self.tree_owner, destination.inode)
                                .map_err(vfs)?
                                .marker_key(),
                            Vec::new(),
                        )
                        .map_err(vfs)?;
                    destination_orphan = Some(destination_key);
                }
            } else {
                decrement_inode_link(&mut planner, self.tree_owner, destination.inode)?;
            }
        }

        let destination_index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .iter()
            .filter(|item| {
                item.key.objectid == destination_parent && item.key.item_type == super::DIR_INDEX
            })
            .map(|item| item.key.offset)
            .max()
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let destination_dir_key = super::TreeItemKey {
            objectid: destination_parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(destination_name.as_bytes())),
        };
        let mut destination_bucket = match planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&destination_dir_key, |item| item.key)
        {
            Ok(_) => load_dir_bucket(&planner, self.tree_owner, destination_dir_key)?,
            Err(_) => Vec::new(),
        };
        destination_bucket
            .try_reserve(1)
            .map_err(|_| VfsError::NoMemory)?;
        let moved = super::BtrfsDirItem {
            inode: source_inode,
            location_type: source.location_type,
            location_offset: source.location_offset,
            item_type: source.item_type,
            transid: generation,
            name: Vec::from(destination_name.as_bytes()),
            data: Vec::new(),
        };
        destination_bucket.push(moved.clone());
        store_dir_bucket(
            &mut planner,
            self.tree_owner,
            destination_dir_key,
            destination_bucket,
        )?;
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: destination_parent,
                    item_type: super::DIR_INDEX,
                    offset: destination_index,
                },
                super::encode_dir_items(&[moved]).map_err(vfs)?,
            )
            .map_err(vfs)?;

        remove_inode_backref(
            &mut planner,
            self.tree_owner,
            source_inode,
            source_parent,
            source_name.as_bytes(),
        )?;
        insert_inode_backref(
            &mut planner,
            self.tree_owner,
            source_inode,
            destination_parent,
            destination_index,
            destination_name.as_bytes(),
            (mount.superblock().nodesize as usize).saturating_sub(0x65 + 25),
        )?;
        if source_kind == NodeType::Directory {
            if source_parent != destination_parent {
                adjust_parent_directory_nlink(
                    &mut planner,
                    self.tree_owner,
                    source_parent,
                    -1,
                    generation,
                )?;
                adjust_parent_directory_nlink(
                    &mut planner,
                    self.tree_owner,
                    destination_parent,
                    1,
                    generation,
                )?;
            }
            if destination_is_directory {
                adjust_parent_directory_nlink(
                    &mut planner,
                    self.tree_owner,
                    destination_parent,
                    -1,
                    generation,
                )?;
            }
        }
        let commit = mount
            .commit_mutation_planner(
                planner,
                0,
                bytes_used.checked_sub(freed).ok_or(VfsError::Io)?,
            )
            .map_err(vfs);
        if commit.is_ok() && destination_orphan.is_some() {
            if let Some(claim) = destination_prepared.as_mut() {
                self.commit_open_orphan(claim);
            }
        }
        commit?;
        Ok(())
    }

    /// Exchanges two existing names in one COW root publication.  Both
    /// DIR_ITEM/DIR_INDEX and INODE_REF pairs are rewritten in the same
    /// planner, so a tree reader cannot observe one half of the swap.
    fn exchange_names(
        &self,
        source_parent: u64,
        source_name: &FsName,
        expected_source: ObjectKey,
        destination_parent: u64,
        destination_name: &FsName,
        expected_destination: ObjectKey,
    ) -> VfsResult<()> {
        if source_name.as_bytes().is_empty()
            || destination_name.as_bytes().is_empty()
            || source_name.as_bytes() == b"."
            || source_name.as_bytes() == b".."
            || destination_name.as_bytes() == b"."
            || destination_name.as_bytes() == b".."
        {
            return Err(VfsError::InvalidInput);
        }
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let source = mount
            .lookup_dir_item(root, self.tree_owner, source_parent, source_name.as_bytes())
            .map_err(vfs)?;
        let destination = mount
            .lookup_dir_item(
                root,
                self.tree_owner,
                destination_parent,
                destination_name.as_bytes(),
            )
            .map_err(vfs)?;
        if source.inode != expected_source.object
            || destination.inode != expected_destination.object
        {
            return Err(VfsError::ResourceBusy);
        }
        let source_item = mount
            .inode_item(root, self.tree_owner, source.inode)
            .map_err(vfs)?;
        let destination_item = mount
            .inode_item(root, self.tree_owner, destination.inode)
            .map_err(vfs)?;
        let source_parent_item = native_inode(&mount, root, self.tree_owner, source_parent)?;
        let destination_parent_item =
            native_inode(&mount, root, self.tree_owner, destination_parent)?;
        admit_native_mutation(&source_parent_item, false)?;
        admit_native_mutation(&destination_parent_item, false)?;
        admit_native_mutation(&source_item, false)?;
        admit_native_mutation(&destination_item, false)?;
        if source_item.generation != expected_source.generation
            || destination_item.generation != expected_destination.generation
        {
            return Err(VfsError::ResourceBusy);
        }
        if source.inode == destination.inode {
            return Ok(());
        }
        // Model both post-exchange parent edges before constructing a
        // planner: either directory may not be reattached below the other.
        // Checking both directions catches A<->descendant swaps and the
        // two-edge cycle that a one-sided rename check misses.
        if kind_from_dir_type(source.item_type) == NodeType::Directory {
            reject_directory_rename_cycle(
                source.inode,
                destination_parent,
                self.root_inode,
                |inode| {
                    mount
                        .directory_parent(root, self.tree_owner, inode, self.root_inode)
                        .map_err(vfs)
                },
            )?;
        }
        if kind_from_dir_type(destination.item_type) == NodeType::Directory {
            reject_directory_rename_cycle(
                destination.inode,
                source_parent,
                self.root_inode,
                |inode| {
                    mount
                        .directory_parent(root, self.tree_owner, inode, self.root_inode)
                        .map_err(vfs)
                },
            )?;
        }
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let bytes_used = mount.superblock().bytes_used;
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;

        // Remove both old items first.  Names with a crc32 collision share a
        // single bucket, so mutate that bucket once rather than writing a
        // stale copy back over the first removal.
        let source_key = super::TreeItemKey {
            objectid: source_parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(source_name.as_bytes())),
        };
        let destination_key = super::TreeItemKey {
            objectid: destination_parent,
            item_type: super::DIR_ITEM,
            offset: u64::from(super::crc32c(destination_name.as_bytes())),
        };
        if source_key == destination_key {
            let mut bucket = load_dir_bucket(&planner, self.tree_owner, source_key)?;
            bucket.retain(|entry| {
                entry.name != source_name.as_bytes() && entry.name != destination_name.as_bytes()
            });
            store_dir_bucket(&mut planner, self.tree_owner, source_key, bucket)?;
        } else {
            let mut bucket = load_dir_bucket(&planner, self.tree_owner, source_key)?;
            bucket.retain(|entry| entry.name != source_name.as_bytes());
            store_dir_bucket(&mut planner, self.tree_owner, source_key, bucket)?;
            let mut bucket = load_dir_bucket(&planner, self.tree_owner, destination_key)?;
            bucket.retain(|entry| entry.name != destination_name.as_bytes());
            store_dir_bucket(&mut planner, self.tree_owner, destination_key, bucket)?;
        }
        let source_index = find_dir_index(
            &planner,
            self.tree_owner,
            source_parent,
            source_name.as_bytes(),
        )?;
        let destination_index = find_dir_index(
            &planner,
            self.tree_owner,
            destination_parent,
            destination_name.as_bytes(),
        )?;
        let _ = planner
            .delete_item(self.tree_owner, source_index)
            .map_err(vfs)?;
        let _ = planner
            .delete_item(self.tree_owner, destination_index)
            .map_err(vfs)?;
        remove_inode_backref(
            &mut planner,
            self.tree_owner,
            source.inode,
            source_parent,
            source_name.as_bytes(),
        )?;
        remove_inode_backref(
            &mut planner,
            self.tree_owner,
            destination.inode,
            destination_parent,
            destination_name.as_bytes(),
        )?;

        let source_new_index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .iter()
            .filter(|item| {
                item.key.objectid == source_parent && item.key.item_type == super::DIR_INDEX
            })
            .map(|item| item.key.offset)
            .max()
            .unwrap_or(1)
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let destination_new_index = if source_parent == destination_parent {
            source_new_index
                .checked_add(1)
                .ok_or(VfsError::StorageFull)?
        } else {
            planner
                .tree_items(self.tree_owner)
                .map_err(vfs)?
                .iter()
                .filter(|item| {
                    item.key.objectid == destination_parent
                        && item.key.item_type == super::DIR_INDEX
                })
                .map(|item| item.key.offset)
                .max()
                .unwrap_or(1)
                .checked_add(1)
                .ok_or(VfsError::StorageFull)?
        };
        let source_new = super::BtrfsDirItem {
            inode: destination.inode,
            location_type: destination.location_type,
            location_offset: destination.location_offset,
            item_type: destination.item_type,
            transid: generation,
            name: Vec::from(source_name.as_bytes()),
            data: Vec::new(),
        };
        let destination_new = super::BtrfsDirItem {
            inode: source.inode,
            location_type: source.location_type,
            location_offset: source.location_offset,
            item_type: source.item_type,
            transid: generation,
            name: Vec::from(destination_name.as_bytes()),
            data: Vec::new(),
        };
        if source_key == destination_key {
            let mut bucket = match planner
                .tree_items(self.tree_owner)
                .map_err(vfs)?
                .binary_search_by_key(&source_key, |item| item.key)
            {
                Ok(_) => load_dir_bucket(&planner, self.tree_owner, source_key)?,
                Err(_) => Vec::new(),
            };
            bucket.try_reserve(2).map_err(|_| VfsError::NoMemory)?;
            bucket.push(source_new.clone());
            bucket.push(destination_new.clone());
            store_dir_bucket(&mut planner, self.tree_owner, source_key, bucket)?;
        } else {
            for (key, item) in [
                (source_key, source_new.clone()),
                (destination_key, destination_new.clone()),
            ] {
                let mut bucket = match planner
                    .tree_items(self.tree_owner)
                    .map_err(vfs)?
                    .binary_search_by_key(&key, |entry| entry.key)
                {
                    Ok(_) => load_dir_bucket(&planner, self.tree_owner, key)?,
                    Err(_) => Vec::new(),
                };
                bucket.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                bucket.push(item);
                store_dir_bucket(&mut planner, self.tree_owner, key, bucket)?;
            }
        }
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: source_parent,
                    item_type: super::DIR_INDEX,
                    offset: source_new_index,
                },
                super::encode_dir_items(&[source_new]).map_err(vfs)?,
            )
            .map_err(vfs)?;
        planner
            .set_item(
                self.tree_owner,
                super::TreeItemKey {
                    objectid: destination_parent,
                    item_type: super::DIR_INDEX,
                    offset: destination_new_index,
                },
                super::encode_dir_items(&[destination_new]).map_err(vfs)?,
            )
            .map_err(vfs)?;
        insert_inode_backref(
            &mut planner,
            self.tree_owner,
            destination.inode,
            source_parent,
            source_new_index,
            source_name.as_bytes(),
            (mount.superblock().nodesize as usize).saturating_sub(0x65 + 25),
        )?;
        insert_inode_backref(
            &mut planner,
            self.tree_owner,
            source.inode,
            destination_parent,
            destination_new_index,
            destination_name.as_bytes(),
            (mount.superblock().nodesize as usize).saturating_sub(0x65 + 25),
        )?;
        if source_parent != destination_parent {
            let source_delta =
                (if kind_from_dir_type(destination.item_type) == NodeType::Directory {
                    1
                } else {
                    0
                }) - (if kind_from_dir_type(source.item_type) == NodeType::Directory {
                    1
                } else {
                    0
                });
            let destination_delta = -source_delta;
            if source_delta != 0 {
                adjust_parent_directory_nlink(
                    &mut planner,
                    self.tree_owner,
                    source_parent,
                    source_delta,
                    generation,
                )?;
            }
            if destination_delta != 0 {
                adjust_parent_directory_nlink(
                    &mut planner,
                    self.tree_owner,
                    destination_parent,
                    destination_delta,
                    generation,
                )?;
            }
        }
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        Ok(())
    }

    /// Replaces a small file's complete native inline extent image.  Inline
    /// conversion is intentionally limited to files which already have no
    /// allocated extent: dropping a regular extent without its matching
    /// extent/checksum/free-space updates would be a corruption bug, not a
    /// harmless short write.  The regular-extent writer takes that larger
    /// cross-tree path separately.
    fn rewrite_inline_file(
        &self,
        inode: u64,
        offset: u64,
        bytes: &[u8],
        truncate_to: Option<u64>,
        typed_append: bool,
    ) -> VfsResult<(usize, u64)> {
        let mut mount = self.mount.lock();
        let root = Self::current_root(&mount, self.tree_owner)?;
        let current = mount
            .inode_item(root, self.tree_owner, inode)
            .map_err(vfs)?;
        let offset = if typed_append { current.size } else { offset };
        // The append entry point reaches here with the mount lock already
        // selecting the actual EOF.  All other rewrites/truncates are denied
        // for append-only inodes before image allocation or media I/O.
        admit_native_mutation(
            &current,
            typed_append && truncate_to.is_none() && offset == current.size,
        )?;
        let final_size = truncate_to
            .unwrap_or_else(|| current.size.max(offset.saturating_add(bytes.len() as u64)));
        let inline_limit = u64::from(mount.superblock().nodesize).saturating_sub(512);
        let extents = mount
            .file_extents(root, self.tree_owner, inode)
            .map_err(vfs)?;
        let has_regular_extent = extents.iter().any(|(_, extent)| {
            extent.kind == super::BtrfsExtentKind::Regular && extent.owns_physical_storage()
        });
        let mut image = Vec::new();
        image
            .try_reserve_exact(
                usize::try_from(current.size.max(final_size)).map_err(|_| VfsError::StorageFull)?,
            )
            .map_err(|_| VfsError::NoMemory)?;
        image.resize(
            usize::try_from(current.size).map_err(|_| VfsError::StorageFull)?,
            0,
        );
        if !image.is_empty() {
            mount
                .read_file_at(root, self.tree_owner, inode, 0, &mut image)
                .map_err(vfs)?;
        }
        image.resize(
            usize::try_from(final_size).map_err(|_| VfsError::StorageFull)?,
            0,
        );
        if !bytes.is_empty() {
            let start = usize::try_from(offset).map_err(|_| VfsError::StorageFull)?;
            let end = start
                .checked_add(bytes.len())
                .ok_or(VfsError::StorageFull)?;
            image[start..end].copy_from_slice(bytes);
        }
        if final_size > inline_limit || has_regular_extent {
            mount
                .replace_file_with_regular(root, self.tree_owner, inode, &image)
                .map_err(vfs)?;
            return Ok((bytes.len(), final_size));
        }
        let generation = mount
            .superblock()
            .generation
            .checked_add(1)
            .ok_or(VfsError::StorageFull)?;
        let bytes_used = mount.superblock().bytes_used;
        let mut planner = mount.mutation_planner(self.tree_owner).map_err(vfs)?;
        let extent_keys: Vec<_> = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .iter()
            .filter(|item| item.key.objectid == inode && item.key.item_type == super::EXTENT_DATA)
            .map(|item| item.key)
            .collect();
        for key in extent_keys {
            let _ = planner.delete_item(self.tree_owner, key).map_err(vfs)?;
        }
        if !image.is_empty() {
            planner
                .set_item(
                    self.tree_owner,
                    super::TreeItemKey {
                        objectid: inode,
                        item_type: super::EXTENT_DATA,
                        offset: 0,
                    },
                    super::encode_inline_extent(generation, &image).map_err(vfs)?,
                )
                .map_err(vfs)?;
        }
        let inode_key = super::TreeItemKey {
            objectid: inode,
            item_type: super::INODE_ITEM,
            offset: 0,
        };
        let inode_index = planner
            .tree_items(self.tree_owner)
            .map_err(vfs)?
            .binary_search_by_key(&inode_key, |item| item.key)
            .map_err(|_| VfsError::NotFound)?;
        let mut inode_item = BtrfsInodeItem::decode(
            &planner.tree_items(self.tree_owner).map_err(vfs)?[inode_index].value,
        )
        .map_err(vfs)?;
        inode_item.transid = generation;
        inode_item.size = final_size;
        inode_item.nbytes = final_size;
        planner
            .set_item(self.tree_owner, inode_key, inode_item.encode())
            .map_err(vfs)?;
        mount
            .commit_mutation_planner(planner, 0, bytes_used)
            .map_err(vfs)?;
        Ok((bytes.len(), final_size))
    }

    fn make_entry(
        self: &Arc<Self>,
        inode: u64,
        kind: NodeType,
        reference: Reference,
    ) -> VfsResult<DirEntry> {
        if kind == NodeType::Directory {
            let fs = self.clone();
            let state = self.inode_state(inode)?;
            return Ok(DirEntry::new_dir(
                move |weak| {
                    DirNode::new(Arc::new(BtrfsInode {
                        fs,
                        inode,
                        state,
                        weak: Some(weak),
                    }))
                },
                reference,
            ));
        }
        let state = self.inode_state(inode)?;
        let node = Arc::try_new(BtrfsInode {
            fs: self.clone(),
            inode,
            state,
            weak: None,
        })
        .map_err(|_| VfsError::NoMemory)?;
        DirEntry::try_new_file(FileNode::new(node), kind, reference)
    }
}

/// Rejects moving a directory below itself.  The caller supplies parents from
/// the same namespace serialization domain as the eventual mutation, so an
/// accepted chain cannot be invalidated by a concurrent rename before commit.
fn reject_directory_rename_cycle(
    source_inode: u64,
    destination_parent: u64,
    root_inode: u64,
    mut parent_of: impl FnMut(u64) -> VfsResult<Option<u64>>,
) -> VfsResult<()> {
    let mut current = destination_parent;
    let mut visited = BTreeSet::new();
    loop {
        if current == source_inode {
            return Err(VfsError::InvalidInput);
        }
        if current == root_inode {
            return Ok(());
        }
        if !visited.insert(current) {
            return Err(VfsError::InvalidData);
        }
        current = parent_of(current)?.ok_or(VfsError::InvalidData)?;
    }
}

#[cfg(test)]
mod rename_ancestry_tests {
    use super::*;

    fn parent_from_table(table: &[(u64, u64)], inode: u64) -> VfsResult<Option<u64>> {
        table
            .iter()
            .find_map(|&(child, parent)| (child == inode).then_some(parent))
            .map(Some)
            .ok_or(VfsError::InvalidData)
    }

    #[test]
    fn rejects_move_beneath_a_descendant() {
        let parents = [(30, 20), (20, 10), (10, 5)];
        assert_eq!(
            reject_directory_rename_cycle(20, 30, 5, |inode| parent_from_table(&parents, inode)),
            Err(VfsError::InvalidInput),
        );
    }

    #[test]
    fn rejects_corrupt_parent_cycle_before_commit() {
        let parents = [(30, 40), (40, 30)];
        assert_eq!(
            reject_directory_rename_cycle(20, 30, 5, |inode| parent_from_table(&parents, inode)),
            Err(VfsError::InvalidData),
        );
    }

    #[test]
    fn accepts_an_unrelated_destination_chain_to_root() {
        let parents = [(30, 10), (10, 5)];
        assert_eq!(
            reject_directory_rename_cycle(20, 30, 5, |inode| parent_from_table(&parents, inode)),
            Ok(()),
        );
    }
}

impl FilesystemOps for BtrfsFilesystem {
    fn name(&self) -> &str {
        "btrfs"
    }
    fn root_dir(&self) -> DirEntry {
        self.root
            .lock()
            .clone()
            .expect("btrfs root installed before publication")
    }
    fn stat(&self) -> VfsResult<StatFs> {
        let superblock = self.mount.lock().superblock();
        Ok(StatFs {
            fs_type: 0x9123_683e,
            block_size: superblock.sectorsize,
            blocks: superblock.total_bytes / u64::from(superblock.sectorsize),
            blocks_free: (superblock.total_bytes - superblock.bytes_used)
                / u64::from(superblock.sectorsize),
            blocks_available: (superblock.total_bytes - superblock.bytes_used)
                / u64::from(superblock.sectorsize),
            file_count: 0,
            free_file_count: 0,
            name_length: 255,
            fragment_size: superblock.sectorsize,
            mount_flags: 0,
        })
    }
    fn flush(&self) -> VfsResult<()> {
        self.mount.lock().volume().flush().map_err(vfs)
    }
}

struct BtrfsInode {
    fs: Arc<BtrfsFilesystem>,
    inode: u64,
    state: Arc<BtrfsInodeState>,
    weak: Option<WeakDirEntry>,
}

/// OFD-private forwarding objects.  Their sole additional state is an open
/// ticket, so dup/fork continue sharing exactly one object and therefore do
/// not inflate the native open count.
struct BtrfsOpenFile {
    inode: Arc<BtrfsInode>,
    ticket: Arc<OpenTicket>,
}
struct BtrfsOpenDirectory {
    inode: Arc<BtrfsInode>,
    ticket: Arc<OpenTicket>,
}

macro_rules! delegate_btrfs_node {
    () => {
        fn inode(&self) -> u64 {
            self.inode.inode()
        }
        fn object_key(&self) -> ObjectKey {
            self.inode.object_key()
        }
        fn metadata(&self) -> VfsResult<Metadata> {
            self.inode.metadata()
        }
        fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
            self.inode.update_metadata(update)
        }
        fn filesystem(&self) -> &dyn FilesystemOps {
            self.inode.filesystem()
        }
        fn sync(&self, data_only: bool) -> VfsResult<()> {
            self.inode.sync(data_only)
        }
        fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
            self
        }
        fn flags(&self) -> NodeFlags {
            self.inode.flags()
        }
        fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
            self.inode.xattr_provider()
        }
        fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
            self.inode.file_attr_provider()
        }
        fn persistent_user_data(&self) -> Option<&NodeUserData> {
            self.inode.persistent_user_data()
        }
        fn lock_ops(&self) -> Option<&dyn LockOps> {
            self.inode.lock_ops()
        }
        fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
            self.inode.set_file_attr(attr)
        }
    };
}

impl NodeOps for BtrfsOpenFile {
    delegate_btrfs_node!();
}
impl Pollable for BtrfsOpenFile {
    fn poll(&self) -> IoEvents {
        self.inode.poll()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.inode.register(context, events)
    }
}
impl FileNodeOps for BtrfsOpenFile {
    fn supports_nowait_read(&self) -> bool {
        self.inode.supports_nowait_read()
    }
    fn supports_nowait_write(&self) -> bool {
        self.inode.supports_nowait_write()
    }
    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        self.inode.mutate_range(request)
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        self.inode.read_at(buf, offset)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        self.inode.write_at(buf, offset)
    }
    fn clone_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        self.inode
            .clone_range_from(source, source_offset, destination_offset, len)
    }
    fn dedupe_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        len: u64,
    ) -> VfsResult<bool> {
        self.inode
            .dedupe_range_from(source, source_offset, destination_offset, len)
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        self.inode.append(buf)
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.inode.set_len(len)
    }
    fn set_len_failure_is_atomic(&self) -> bool {
        self.inode.set_len_failure_is_atomic()
    }
    fn set_symlink(&self, target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        self.inode.set_symlink(target)
    }
    fn release_handle(&self) -> VfsResult<()> {
        self.ticket.release();
        Ok(())
    }
}
impl NodeOps for BtrfsOpenDirectory {
    delegate_btrfs_node!();
}
impl Pollable for BtrfsOpenDirectory {
    fn poll(&self) -> IoEvents {
        self.inode.poll()
    }
    fn register<'a>(
        &'a self,
        context: &mut Context<'_>,
        events: IoEvents,
    ) -> Result<PollRegistration<'a>, PollRegistrationError> {
        self.inode.register(context, events)
    }
}
impl DirNodeOps for BtrfsOpenDirectory {
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        self.inode.supports_named_create(node_type)
    }
    fn supports_symlink(&self) -> bool {
        self.inode.supports_symlink()
    }
    fn supports_hard_links(&self) -> bool {
        self.inode.supports_hard_links()
    }
    fn supports_unlink(&self) -> bool {
        self.inode.supports_unlink()
    }
    fn supports_rmdir(&self) -> bool {
        self.inode.supports_rmdir()
    }
    fn supports_rename(&self) -> bool {
        self.inode.supports_rename()
    }
    fn supports_rename_exchange(&self) -> bool {
        self.inode.supports_rename_exchange()
    }
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        self.inode.read_dir(offset, sink)
    }
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        self.inode.lookup(name)
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
        disposition: axfs_ng_vfs::CreateDisposition,
    ) -> VfsResult<axfs_ng_vfs::CreateOutcome<DirEntry>> {
        self.inode.create_named(name, options, disposition)
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: axfs_ng_vfs::NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        self.inode.create_symlink(name, target, permission, user)
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        options: &axfs_ng_vfs::NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        self.inode.create_symlink_prepared(name, target, options)
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        self.inode.link(name, node)
    }
    fn unlink(&self, request: axfs_ng_vfs::UnlinkRequest<'_>) -> VfsResult<()> {
        self.inode.unlink(request)
    }
    fn rename(&self, request: axfs_ng_vfs::RenameRequest<'_>) -> VfsResult<()> {
        self.inode.rename(request)
    }
    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        self.inode.rename_exchange(request)
    }
    fn release_handle(&self) -> VfsResult<()> {
        self.ticket.release();
        Ok(())
    }
}

impl BtrfsInode {
    fn item(&self) -> VfsResult<BtrfsInodeItem> {
        self.fs.inode_item(self.inode)
    }
    fn entry(&self, name: &FsName, inode: u64, kind: NodeType) -> VfsResult<DirEntry> {
        let parent = self
            .weak
            .as_ref()
            .and_then(WeakDirEntry::upgrade)
            .ok_or(VfsError::Io)?;
        let actual = self
            .fs
            .inode_item(inode)?
            .metadata(inode, self.fs.device_id, 0)
            .map_err(vfs)?
            .node_type;
        if actual != kind {
            return Err(VfsError::Io);
        }
        self.fs
            .make_entry(inode, kind, Reference::try_new(Some(parent), name)?)
    }
}

impl NodeOps for BtrfsInode {
    fn inode(&self) -> u64 {
        self.inode
    }
    fn object_key(&self) -> ObjectKey {
        match self.item() {
            Ok(item) => ObjectKey::new(self.fs.device_id, self.inode, item.generation),
            Err(_) => ObjectKey::new(self.fs.device_id, self.inode, 0),
        }
    }
    fn metadata(&self) -> VfsResult<Metadata> {
        self.item()?
            .metadata(self.inode, self.fs.device_id, 0)
            .map_err(vfs)
    }
    fn update_metadata(&self, update: MetadataUpdate) -> VfsResult<()> {
        self.fs.persist_metadata(self.inode, update)
    }
    fn filesystem(&self) -> &dyn FilesystemOps {
        self.fs.as_ref()
    }
    fn sync(&self, data_only: bool) -> VfsResult<()> {
        if data_only {
            return self.fs.flush();
        }
        // Native multi-root tree-log writing is deliberately not implemented
        // in this phase.  Never substitute the retired single-tree format or
        // claim full fsync durability by a flush that lacks its log record.
        Err(VfsError::OperationNotSupported)
    }
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
    fn flags(&self) -> NodeFlags {
        NodeFlags::BLOCKING
    }
    fn xattr_provider(&self) -> Option<&dyn XattrProvider> {
        Some(self)
    }
    fn file_attr_provider(&self) -> Option<&dyn FileAttrProvider> {
        Some(self.state.as_ref())
    }
    fn persistent_user_data(&self) -> Option<&NodeUserData> {
        Some(self.state.persistent_user_data())
    }
    fn lock_ops(&self) -> Option<&dyn LockOps> {
        Some(self.state.as_ref())
    }
    fn set_file_attr(&self, attr: FileAttr) -> VfsResult<()> {
        BtrfsInodeState::validate_file_attr(attr)?;
        self.fs.persist_file_attr(self.inode, attr)?;
        Ok(())
    }
}

impl XattrProvider for BtrfsInode {
    fn get_xattr(&self, name: &[u8]) -> VfsResult<Vec<u8>> {
        let mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        mount
            .get_xattr(root, self.fs.tree_owner, self.inode, name)
            .map_err(vfs)
    }
    fn list_xattrs(&self) -> VfsResult<Vec<u8>> {
        let mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        mount
            .list_xattrs(root, self.fs.tree_owner, self.inode)
            .map_err(vfs)
    }
    fn set_xattr(&self, name: &[u8], value: &[u8], mode: XattrSetMode) -> VfsResult<()> {
        self.fs.persist_xattr(self.inode, name, Some(value), mode)
    }
    fn remove_xattr(&self, name: &[u8]) -> VfsResult<()> {
        self.fs
            .persist_xattr(self.inode, name, None, XattrSetMode::Upsert)
    }
}

impl Pollable for BtrfsInode {
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

impl FileNodeOps for BtrfsInode {
    fn open_handle(
        &self,
        _read: bool,
        _write: bool,
        _flags: u32,
    ) -> VfsResult<Option<Arc<dyn FileNodeOps>>> {
        let ticket = self.fs.acquire_open_ticket(self.inode)?;
        let inode = Arc::try_new(BtrfsInode {
            fs: self.fs.clone(),
            inode: self.inode,
            state: self.state.clone(),
            weak: self.weak.clone(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(
            Arc::try_new(BtrfsOpenFile { inode, ticket }).map_err(|_| VfsError::NoMemory)?,
        ))
    }
    fn supports_nowait_read(&self) -> bool {
        true
    }
    fn supports_nowait_write(&self) -> bool {
        true
    }

    fn mutate_range(&self, request: FileRangeRequest) -> VfsResult<()> {
        // One mount critical section covers the inode size, extent snapshot,
        // range plan, and mutation commit.  Sampling i_size before taking the
        // mount mutex can otherwise combine an old size with a new extent
        // tree after a concurrent write/truncate and silently lose either
        // side's update.
        let mut mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        let inode_item = mount
            .inode_item(root, self.fs.tree_owner, self.inode)
            .map_err(vfs)?;
        admit_native_mutation(&inode_item, false)?;
        let size = inode_item.size;
        let end = request.end();
        // The common punch-beyond-EOF case is specified as a no-op.  More
        // importantly, do it before collecting extent metadata: fallocate on
        // a sparse multi-terabyte inode must not turn an O(1) no-op into a
        // file-sized allocation or scan.
        if matches!(request.operation, FileRangeOperation::PunchHole) && request.offset >= size {
            return Ok(());
        }
        let final_size = match request.operation {
            FileRangeOperation::Allocate { keep_size }
            | FileRangeOperation::ZeroRange { keep_size } => {
                if keep_size {
                    size
                } else {
                    size.max(end)
                }
            }
            FileRangeOperation::PunchHole | FileRangeOperation::UnshareRange => size,
            FileRangeOperation::CollapseRange => {
                if end > size {
                    return Err(VfsError::InvalidInput);
                }
                size - request.length
            }
            FileRangeOperation::InsertRange => {
                if request.offset >= size {
                    return Err(VfsError::InvalidInput);
                }
                size.checked_add(request.length)
                    .ok_or(VfsError::StorageFull)?
            }
        };
        let sector = u64::from(mount.superblock().sectorsize);
        if matches!(
            request.operation,
            FileRangeOperation::CollapseRange | FileRangeOperation::InsertRange
        ) && (!request.offset.is_multiple_of(sector) || !request.length.is_multiple_of(sector))
        {
            return Err(VfsError::InvalidInput);
        }
        if matches!(request.operation, FileRangeOperation::UnshareRange) && end > size {
            return Err(VfsError::InvalidInput);
        }

        let extents = mount
            .file_extents(root, self.fs.tree_owner, self.inode)
            .map_err(vfs)?;
        let mut segments: Vec<super::RangeSegment> = Vec::new();
        // Materialize only an individual affected extent interval.  The old
        // implementation allocated `max(i_size, final_size)` and read the
        // entire inode, which made a one-sector punch/insert proportional to
        // file size and overflowed usize for perfectly valid sparse files.
        macro_rules! copy_piece {
            ($source:expr, $destination:expr, $length:expr) => {{
                let source = $source;
                let destination = $destination;
                let length = $length;
                if length != 0 {
                    segments.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    segments.push(super::RangeSegment::Retain {
                        source_inode: self.inode,
                        source_offset: source,
                        destination_offset: destination,
                        length,
                    });
                }
            }};
        }
        macro_rules! zero_piece {
            ($destination:expr, $length:expr) => {{
                let destination = $destination;
                let length = $length;
                if length != 0 {
                    let length = usize::try_from(length).map_err(|_| VfsError::StorageFull)?;
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve_exact(length)
                        .map_err(|_| VfsError::NoMemory)?;
                    bytes.resize(length, 0);
                    segments.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    segments.push(super::RangeSegment::CowData {
                        offset: destination,
                        bytes,
                    });
                }
            }};
        }
        // fallocate(ALLOCATE) has a distinct on-disk meaning from a zero
        // write: it reserves blocks without data I/O or checksum items.
        // Keep that provenance through the planner instead of materializing
        // an arbitrarily large zero Vec for sparse holes.
        macro_rules! prealloc_piece {
            ($offset:expr, $length:expr) => {{
                let offset = $offset;
                let length = $length;
                if length != 0 {
                    segments.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    segments.push(super::RangeSegment::Prealloc { offset, length });
                }
            }};
        }
        macro_rules! hole_piece {
            ($offset:expr, $length:expr) => {{
                let offset = $offset;
                let length = $length;
                if length != 0 {
                    segments.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    segments.push(super::RangeSegment::Hole { offset, length });
                }
            }};
        }
        macro_rules! cow_piece {
            ($source:expr, $destination:expr, $length:expr) => {{
                let source = $source;
                let destination = $destination;
                let length = $length;
                if length != 0 {
                    let length = usize::try_from(length).map_err(|_| VfsError::StorageFull)?;
                    let mut bytes = Vec::new();
                    bytes
                        .try_reserve_exact(length)
                        .map_err(|_| VfsError::NoMemory)?;
                    bytes.resize(length, 0);
                    mount
                        .read_file_at(root, self.fs.tree_owner, self.inode, source, &mut bytes)
                        .map_err(vfs)?;
                    segments.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
                    segments.push(super::RangeSegment::CowData {
                        offset: destination,
                        bytes,
                    });
                }
            }};
        }

        match request.operation {
            FileRangeOperation::CollapseRange => {
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    if extent_end <= request.offset {
                        copy_piece!(*offset, *offset, extent.num_bytes);
                    } else if *offset >= end {
                        copy_piece!(*offset, offset - request.length, extent.num_bytes);
                    } else {
                        if *offset < request.offset {
                            copy_piece!(*offset, *offset, request.offset - *offset);
                        }
                        if extent_end > end {
                            copy_piece!(end, request.offset, extent_end - end);
                        }
                    }
                }
            }
            FileRangeOperation::InsertRange => {
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    if extent_end <= request.offset {
                        copy_piece!(*offset, *offset, extent.num_bytes);
                    } else if *offset >= request.offset {
                        copy_piece!(
                            *offset,
                            offset
                                .checked_add(request.length)
                                .ok_or(VfsError::StorageFull)?,
                            extent.num_bytes
                        );
                    } else {
                        copy_piece!(*offset, *offset, request.offset - *offset);
                        copy_piece!(
                            request.offset,
                            request
                                .offset
                                .checked_add(request.length)
                                .ok_or(VfsError::StorageFull)?,
                            extent_end - request.offset
                        );
                    }
                }
            }
            FileRangeOperation::PunchHole => {
                let punched_end = end.min(size);
                let full_start = request.offset.div_ceil(sector).saturating_mul(sector);
                let full_end = (punched_end / sector).saturating_mul(sector);
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    if extent_end <= request.offset || *offset >= punched_end {
                        copy_piece!(*offset, *offset, extent.num_bytes);
                        continue;
                    }
                    let left = request.offset.max(*offset);
                    if left > *offset {
                        copy_piece!(*offset, *offset, left - *offset);
                    }
                    if full_start < full_end {
                        let left_edge_end = full_start.min(punched_end).min(extent_end).max(left);
                        if left_edge_end > left {
                            zero_piece!(left, left_edge_end - left);
                        }
                        let hole_start = full_start.max(*offset);
                        let hole_end = full_end.min(extent_end);
                        if hole_end > hole_start {
                            hole_piece!(hole_start, hole_end - hole_start);
                        }
                        let right_edge_start =
                            full_end.max(request.offset).max(*offset).min(punched_end);
                        let right_edge_end = punched_end.min(extent_end);
                        if right_edge_end > right_edge_start {
                            zero_piece!(right_edge_start, right_edge_end - right_edge_start);
                        }
                        if extent_end > right_edge_end {
                            copy_piece!(
                                right_edge_end,
                                right_edge_end,
                                extent_end - right_edge_end
                            );
                        }
                    } else {
                        let zero_end = punched_end.min(extent_end);
                        if zero_end > left {
                            zero_piece!(left, zero_end - left);
                        }
                        if extent_end > zero_end {
                            copy_piece!(zero_end, zero_end, extent_end - zero_end);
                        }
                    }
                }
            }
            FileRangeOperation::ZeroRange { keep_size } => {
                let zero_end = if keep_size { end.min(size) } else { end };
                let mut cursor = request.offset;
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    if extent_end <= request.offset || *offset >= zero_end {
                        copy_piece!(*offset, *offset, extent.num_bytes);
                        continue;
                    }
                    if cursor < *offset {
                        zero_piece!(cursor, (*offset).min(zero_end) - cursor);
                    }
                    if *offset < request.offset {
                        copy_piece!(*offset, *offset, request.offset - *offset);
                    }
                    let overlap_start = (*offset).max(request.offset);
                    let overlap_end = extent_end.min(zero_end);
                    zero_piece!(overlap_start, overlap_end - overlap_start);
                    if extent_end > zero_end {
                        copy_piece!(zero_end, zero_end, extent_end - zero_end);
                    }
                    cursor = cursor.max(overlap_end);
                }
                if cursor < zero_end {
                    zero_piece!(cursor, zero_end - cursor);
                }
            }
            FileRangeOperation::Allocate { .. } => {
                let mut cursor = request.offset;
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    copy_piece!(*offset, *offset, extent.num_bytes);
                    if extent_end <= request.offset || *offset >= end {
                        continue;
                    }
                    if cursor < *offset {
                        prealloc_piece!(cursor, (*offset).min(end) - cursor);
                    }
                    cursor = cursor.max(extent_end.min(end));
                }
                if cursor < end {
                    prealloc_piece!(cursor, end - cursor);
                }
            }
            FileRangeOperation::UnshareRange => {
                for (offset, extent) in &extents {
                    let extent_end = offset
                        .checked_add(extent.num_bytes)
                        .ok_or(VfsError::InvalidData)?;
                    if extent_end <= request.offset || *offset >= end {
                        copy_piece!(*offset, *offset, extent.num_bytes);
                    } else {
                        let changed_start = (*offset).max(request.offset);
                        let changed_end = extent_end.min(end);
                        if changed_start > *offset {
                            copy_piece!(*offset, *offset, changed_start - *offset);
                        }
                        cow_piece!(changed_start, changed_start, changed_end - changed_start);
                        if extent_end > changed_end {
                            copy_piece!(changed_end, changed_end, extent_end - changed_end);
                        }
                    }
                }
            }
        }
        segments.sort_by_key(super::RangeSegment::offset);
        mount
            .replace_file_with_range_segments(
                root,
                self.fs.tree_owner,
                self.inode,
                final_size,
                &segments,
            )
            .map_err(vfs)?;
        Ok(())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        mount
            .read_file_at(root, self.fs.tree_owner, self.inode, offset, buf)
            .map_err(vfs)
    }
    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.fs
            .rewrite_inline_file(self.inode, offset, buf, None, false)
            .map(|(written, _)| written)
    }
    fn clone_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        len: u64,
    ) -> VfsResult<()> {
        if source.object_key().filesystem != self.fs.device_id {
            return Err(VfsError::CrossesDevices);
        }
        let mut mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        admit_native_mutation(
            &native_inode(&mount, root, self.fs.tree_owner, self.inode)?,
            false,
        )?;
        mount
            .reflink_regular_extent(
                root,
                self.fs.tree_owner,
                source.inode(),
                source_offset,
                self.inode,
                destination_offset,
                len,
            )
            .map_err(vfs)
    }
    fn dedupe_range_from(
        &self,
        source: &dyn NodeOps,
        source_offset: u64,
        destination_offset: u64,
        len: u64,
    ) -> VfsResult<bool> {
        if source.object_key().filesystem != self.fs.device_id {
            return Err(VfsError::CrossesDevices);
        }
        let mut mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        admit_native_mutation(
            &native_inode(&mount, root, self.fs.tree_owner, self.inode)?,
            false,
        )?;
        mount
            .dedupe_regular_extent(
                root,
                self.fs.tree_owner,
                source.inode(),
                source_offset,
                self.inode,
                destination_offset,
                len,
            )
            .map_err(vfs)
    }
    fn append(&self, buf: &[u8]) -> VfsResult<(usize, u64)> {
        // EOF selection and the data mutation occur under one mount snapshot.
        // No size is sampled in this caller, so concurrent appenders serialize
        // rather than producing a stale-offset overwrite or spurious EAGAIN.
        self.fs.rewrite_inline_file(self.inode, 0, buf, None, true)
    }
    fn set_len(&self, len: u64) -> VfsResult<()> {
        self.fs
            .rewrite_inline_file(self.inode, len, &[], Some(len), false)?;
        Ok(())
    }
    fn set_len_failure_is_atomic(&self) -> bool {
        true
    }
    fn set_symlink(&self, target: &axfs_ng_vfs::FsPath) -> VfsResult<()> {
        if self
            .item()?
            .metadata(self.inode, self.fs.device_id, 0)
            .map_err(vfs)?
            .node_type
            != NodeType::Symlink
        {
            return Err(VfsError::InvalidInput);
        }
        if target.as_bytes().is_empty() {
            return Err(VfsError::InvalidInput);
        }
        self.fs.rewrite_inline_file(
            self.inode,
            0,
            target.as_bytes(),
            Some(target.as_bytes().len() as u64),
            false,
        )?;
        Ok(())
    }
}

impl DirNodeOps for BtrfsInode {
    fn open_handle(&self, _flags: u32) -> VfsResult<Option<Arc<dyn DirNodeOps>>> {
        let ticket = self.fs.acquire_open_ticket(self.inode)?;
        let inode = Arc::try_new(BtrfsInode {
            fs: self.fs.clone(),
            inode: self.inode,
            state: self.state.clone(),
            weak: self.weak.clone(),
        })
        .map_err(|_| VfsError::NoMemory)?;
        Ok(Some(
            Arc::try_new(BtrfsOpenDirectory { inode, ticket }).map_err(|_| VfsError::NoMemory)?,
        ))
    }
    fn supports_named_create(&self, node_type: NodeType) -> bool {
        matches!(node_type, NodeType::RegularFile | NodeType::Directory)
    }
    fn supports_symlink(&self) -> bool {
        true
    }
    fn supports_hard_links(&self) -> bool {
        true
    }
    fn supports_unlink(&self) -> bool {
        true
    }
    fn supports_rmdir(&self) -> bool {
        true
    }
    fn read_dir(&self, offset: u64, sink: &mut dyn DirEntrySink) -> VfsResult<usize> {
        let mut count = 0;
        if offset == 0 && sink.accept(FsName::new(b"."), self.inode, NodeType::Directory, 1) {
            count += 1;
        }
        if offset <= 1 && sink.accept(FsName::new(b".."), self.inode, NodeType::Directory, 2) {
            count += 1;
        }
        let mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        let entries = mount
            .directory_items(root, self.fs.tree_owner, self.inode)
            .map_err(vfs)?;
        for (index, entry) in entries
            .into_iter()
            .enumerate()
            .skip(offset.saturating_sub(2) as usize)
        {
            let next = index as u64 + 3;
            if !sink.accept(
                FsName::new(&entry.name),
                entry.inode,
                kind_from_dir_type(entry.item_type),
                next,
            ) {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
    fn lookup(&self, name: &FsName) -> VfsResult<DirEntry> {
        let mount = self.fs.mount.lock();
        let root = BtrfsFilesystem::current_root(&mount, self.fs.tree_owner)?;
        let entry = mount
            .lookup_dir_item(root, self.fs.tree_owner, self.inode, name.as_bytes())
            .map_err(vfs)?;
        self.entry(name, entry.inode, kind_from_dir_type(entry.item_type))
    }
    fn create_named(
        &self,
        name: &FsName,
        options: &axfs_ng_vfs::NamedCreateOptions,
        disposition: axfs_ng_vfs::CreateDisposition,
    ) -> VfsResult<axfs_ng_vfs::CreateOutcome<DirEntry>> {
        let (inode, created) = self.fs.create_inode(self.inode, name, options, None)?;
        if !created && disposition == axfs_ng_vfs::CreateDisposition::Exclusive {
            return Err(VfsError::AlreadyExists);
        }
        let entry = self.entry(name, inode, options.node_type)?;
        if created {
            options.install_initial_data(&entry)?;
        }
        Ok(axfs_ng_vfs::CreateOutcome { entry, created })
    }
    fn create_symlink(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        permission: axfs_ng_vfs::NodePermission,
        user: Option<(u32, u32)>,
    ) -> VfsResult<DirEntry> {
        let options = axfs_ng_vfs::NamedCreateOptions {
            node_type: NodeType::Symlink,
            permission,
            owner: user,
            rdev: None,
            initial_data: None,
            initial_attributes: Default::default(),
        };
        let (inode, created) = self
            .fs
            .create_inode(self.inode, name, &options, Some(target))?;
        if !created {
            return Err(VfsError::AlreadyExists);
        }
        self.entry(name, inode, NodeType::Symlink)
    }
    fn create_symlink_prepared(
        &self,
        name: &FsName,
        target: &axfs_ng_vfs::FsPath,
        options: &axfs_ng_vfs::NamedCreateOptions,
    ) -> VfsResult<DirEntry> {
        if options.node_type != NodeType::Symlink
            || options.initial_attributes.project_inherit
            || options.initial_attributes.default_acl.is_some()
        {
            return Err(VfsError::InvalidInput);
        }
        // `create_inode` places the inode item, access ACL xattr, inline
        // target, and directory item in one planner commit.  Passing the
        // original prepared options is therefore materially different from
        // the legacy symlink helper, which cannot carry initial attributes.
        let (inode, created) = self
            .fs
            .create_inode(self.inode, name, options, Some(target))?;
        if !created {
            return Err(VfsError::AlreadyExists);
        }
        self.entry(name, inode, NodeType::Symlink)
    }
    fn link(&self, name: &FsName, node: &DirEntry) -> VfsResult<DirEntry> {
        let target = node
            .downcast::<BtrfsInode>()
            .map_err(|_| VfsError::CrossesDevices)?;
        self.fs.link_inode(self.inode, name, &target)?;
        self.entry(name, target.inode, node.node_type())
    }
    fn unlink(&self, request: axfs_ng_vfs::UnlinkRequest<'_>) -> VfsResult<()> {
        self.fs.unlink_name(
            self.inode,
            request.name,
            request.expected.map(DirEntry::inode),
            request.is_dir,
        )
    }
    fn supports_rename(&self) -> bool {
        true
    }
    fn supports_rename_exchange(&self) -> bool {
        true
    }
    fn rename(&self, request: axfs_ng_vfs::RenameRequest<'_>) -> VfsResult<()> {
        let destination = request.dst_dir.downcast::<BtrfsInode>()?;
        if !Arc::ptr_eq(&self.fs, &destination.fs) {
            return Err(VfsError::CrossesDevices);
        }
        self.fs.rename_name(
            self.inode,
            request.src_name,
            request.src.inode(),
            destination.inode,
            request.dst_name,
            request.dst.map(DirEntry::inode),
        )
    }
    fn rename_exchange(&self, request: axfs_ng_vfs::RenameExchangeRequest<'_>) -> VfsResult<()> {
        let destination = request.dst_dir.downcast::<BtrfsInode>()?;
        if !Arc::ptr_eq(&self.fs, &destination.fs) {
            return Err(VfsError::CrossesDevices);
        }
        self.fs.exchange_names(
            self.inode,
            request.src_name,
            request.src.object_key(),
            destination.inode,
            request.dst_name,
            request.dst.object_key(),
        )
    }
}

fn load_dir_bucket(
    planner: &super::BtrfsMutationPlanner,
    tree: u64,
    key: super::TreeItemKey,
) -> VfsResult<Vec<super::BtrfsDirItem>> {
    let items = planner.tree_items(tree).map_err(vfs)?;
    let index = items
        .binary_search_by_key(&key, |item| item.key)
        .map_err(|_| VfsError::NotFound)?;
    super::decode_dir_items(&items[index].value).map_err(vfs)
}

fn store_dir_bucket(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    key: super::TreeItemKey,
    bucket: Vec<super::BtrfsDirItem>,
) -> VfsResult<()> {
    if bucket.is_empty() {
        let _ = planner.delete_item(tree, key).map_err(vfs)?;
    } else {
        planner
            .set_item(tree, key, super::encode_dir_items(&bucket).map_err(vfs)?)
            .map_err(vfs)?;
    }
    Ok(())
}

fn find_dir_index(
    planner: &super::BtrfsMutationPlanner,
    tree: u64,
    parent: u64,
    name: &[u8],
) -> VfsResult<super::TreeItemKey> {
    planner
        .tree_items(tree)
        .map_err(vfs)?
        .iter()
        .find(|item| {
            item.key.objectid == parent
                && item.key.item_type == super::DIR_INDEX
                && super::decode_dir_items(&item.value)
                    .ok()
                    .is_some_and(|entries| entries.iter().any(|entry| entry.name == name))
        })
        .map(|item| item.key)
        .ok_or(VfsError::NotFound)
}

fn remove_inode_backref(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    inode: u64,
    parent: u64,
    name: &[u8],
) -> VfsResult<()> {
    let regular = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_REF,
        offset: parent,
    };
    if let Ok(position) = planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&regular, |item| item.key)
    {
        let mut refs =
            super::decode_inode_refs(&planner.tree_items(tree).map_err(vfs)?[position].value)
                .map_err(vfs)?;
        let before = refs.len();
        refs.retain(|reference| reference.name.as_slice() != name);
        if refs.len() + 1 == before {
            if refs.is_empty() {
                let _ = planner.delete_item(tree, regular).map_err(vfs)?;
            } else {
                planner
                    .set_item(tree, regular, super::encode_inode_refs(&refs).map_err(vfs)?)
                    .map_err(vfs)?;
            }
            return Ok(());
        }
    }
    let extended = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_EXTREF,
        offset: super::btrfs_extref_hash(parent, name),
    };
    let position = planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&extended, |item| item.key)
        .map_err(|_| VfsError::NotFound)?;
    let mut records =
        super::decode_inode_extrefs(&planner.tree_items(tree).map_err(vfs)?[position].value)
            .map_err(vfs)?;
    let before = records.len();
    records.retain(|(record_parent, _, record_name)| {
        *record_parent != parent || record_name.as_slice() != name
    });
    if before == records.len() {
        return Err(VfsError::NotFound);
    }
    if records.is_empty() {
        let _ = planner.delete_item(tree, extended).map_err(vfs)?;
    } else {
        planner
            .set_item(
                tree,
                extended,
                super::encode_inode_extrefs(&records).map_err(vfs)?,
            )
            .map_err(vfs)?;
    }
    Ok(())
}

fn insert_inode_backref(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    inode: u64,
    parent: u64,
    index: u64,
    name: &[u8],
    ordinary_limit: usize,
) -> VfsResult<()> {
    let regular = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_REF,
        offset: parent,
    };
    let mut ordinary = match planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&regular, |item| item.key)
    {
        Ok(position) => {
            super::decode_inode_refs(&planner.tree_items(tree).map_err(vfs)?[position].value)
                .map_err(vfs)?
        }
        Err(_) => Vec::new(),
    };
    if ordinary
        .iter()
        .any(|reference| reference.name.as_slice() == name)
    {
        return Err(VfsError::AlreadyExists);
    }
    ordinary.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    ordinary.push(super::BtrfsInodeRef {
        index,
        name: Vec::from(name),
    });
    if ordinary_limit >= 10 {
        let encoded = super::encode_inode_refs(&ordinary).map_err(vfs)?;
        if encoded.len() <= ordinary_limit {
            return planner.set_item(tree, regular, encoded).map_err(vfs);
        }
    }
    let extended = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_EXTREF,
        offset: super::btrfs_extref_hash(parent, name),
    };
    let mut records = match planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&extended, |item| item.key)
    {
        Ok(position) => {
            super::decode_inode_extrefs(&planner.tree_items(tree).map_err(vfs)?[position].value)
                .map_err(vfs)?
        }
        Err(_) => Vec::new(),
    };
    if records.iter().any(|(record_parent, _, record_name)| {
        *record_parent == parent && record_name.as_slice() == name
    }) {
        return Err(VfsError::AlreadyExists);
    }
    records.try_reserve(1).map_err(|_| VfsError::NoMemory)?;
    records.push((parent, index, Vec::from(name)));
    planner
        .set_item(
            tree,
            extended,
            super::encode_inode_extrefs(&records).map_err(vfs)?,
        )
        .map_err(vfs)
}

/// Parent directories count their direct directory children in addition to
/// the native two-link baseline.  All callers run inside one mutation
/// planner, so an error leaves both names and counts unpublished.
fn adjust_parent_directory_nlink(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    parent: u64,
    delta: i32,
    generation: u64,
) -> VfsResult<()> {
    let key = super::TreeItemKey {
        objectid: parent,
        item_type: super::INODE_ITEM,
        offset: 0,
    };
    let position = planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&key, |item| item.key)
        .map_err(|_| VfsError::NotFound)?;
    let mut inode = BtrfsInodeItem::decode(&planner.tree_items(tree).map_err(vfs)?[position].value)
        .map_err(vfs)?;
    if inode.mode & 0o170000 != 0o040000 || generation == 0 {
        return Err(VfsError::InvalidInput);
    }
    inode.nlink = if delta >= 0 {
        inode
            .nlink
            .checked_add(u32::try_from(delta).map_err(|_| VfsError::StorageFull)?)
            .ok_or(VfsError::StorageFull)?
    } else {
        if inode.nlink <= 2 {
            return Err(VfsError::Io);
        }
        inode
            .nlink
            .checked_sub(delta.unsigned_abs())
            .ok_or(VfsError::Io)?
    };
    if inode.nlink < 2 {
        return Err(VfsError::Io);
    }
    inode.transid = generation;
    planner.set_item(tree, key, inode.encode()).map_err(vfs)
}

/// Retires the two intrinsic links of an already-emptied directory after its
/// sole parent/name edge has been removed.  This is not the ordinary file
/// unlink decrement: an empty directory's `nlink == 2` represents `.` and
/// its parent relation.  Any remaining data extent would need shared extent
/// accounting, so it is rejected instead of silently orphaning storage.
fn retire_empty_directory_links(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    inode: u64,
) -> VfsResult<()> {
    let inode_key = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_ITEM,
        offset: 0,
    };
    let position = planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&inode_key, |item| item.key)
        .map_err(|_| VfsError::NotFound)?;
    let item = BtrfsInodeItem::decode(&planner.tree_items(tree).map_err(vfs)?[position].value)
        .map_err(vfs)?;
    if item.mode & 0o170000 != 0o040000 || item.nlink != 2 {
        return Err(VfsError::Io);
    }
    if planner.tree_items(tree).map_err(vfs)?.iter().any(|item| {
        item.key.objectid == inode
            && matches!(
                item.key.item_type,
                super::DIR_ITEM | super::DIR_INDEX | super::EXTENT_DATA
            )
    }) {
        return Err(VfsError::Io);
    }
    let keys: Vec<super::TreeItemKey> = planner
        .tree_items(tree)
        .map_err(vfs)?
        .iter()
        .filter_map(|item| {
            if item.key.objectid != inode {
                return None;
            }
            matches!(
                item.key.item_type,
                super::INODE_ITEM | super::INODE_REF | super::INODE_EXTREF | super::XATTR_ITEM
            )
            .then_some(item.key)
        })
        .collect();
    if !keys.contains(&inode_key) {
        return Err(VfsError::Io);
    }
    for key in keys {
        let _ = planner.delete_item(tree, key).map_err(vfs)?;
    }
    Ok(())
}

/// Last-link inode cleanup stages every physical retirement before deleting
/// the filesystem-tree keys.  The caller finishes the shared free-space
/// image and commits this same planner, so no data ref/checksum/qgroup state
/// can outlive the inode which named it.
fn retire_last_link_inode(
    mount: &BtrfsMount,
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    inode: u64,
    generation: u64,
    free_space: &super::BtrfsLogicalAllocator,
) -> VfsResult<u64> {
    let sector = u64::from(mount.superblock().sectorsize);
    let mut freed = 0u64;
    let extents: Vec<(super::TreeItemKey, Vec<u8>)> = planner
        .tree_items(tree)
        .map_err(vfs)?
        .iter()
        .filter(|item| item.key.objectid == inode && item.key.item_type == super::EXTENT_DATA)
        .map(|item| (item.key, item.value.clone()))
        .collect();
    for (key, value) in extents {
        let retirement = super::LoggedExtentRetirement::decode(tree, key, &value).map_err(vfs)?;
        let released = planner
            .prepare_logged_extent_retirement(&retirement, generation, sector, free_space)
            .map_err(vfs)?;
        freed = freed.checked_add(released).ok_or(VfsError::StorageFull)?;
    }
    let marker = super::OrphanRetirement::new(tree, inode)
        .map_err(vfs)?
        .marker_key();
    planner.set_item(tree, marker, Vec::new()).map_err(vfs)?;
    let keys: Vec<super::TreeItemKey> = planner
        .tree_items(tree)
        .map_err(vfs)?
        .iter()
        .filter_map(|item| {
            (item.key.objectid == inode
                && matches!(
                    item.key.item_type,
                    super::INODE_ITEM
                        | super::INODE_REF
                        | super::INODE_EXTREF
                        | super::XATTR_ITEM
                        | super::EXTENT_DATA
                ))
            .then_some(item.key)
        })
        .collect();
    for key in keys {
        let _ = planner.delete_item(tree, key).map_err(vfs)?;
    }
    let _ = planner.delete_item(tree, marker).map_err(vfs)?;
    Ok(freed)
}

fn decrement_inode_link(
    planner: &mut super::BtrfsMutationPlanner,
    tree: u64,
    inode: u64,
) -> VfsResult<()> {
    let key = super::TreeItemKey {
        objectid: inode,
        item_type: super::INODE_ITEM,
        offset: 0,
    };
    let index = planner
        .tree_items(tree)
        .map_err(vfs)?
        .binary_search_by_key(&key, |item| item.key)
        .map_err(|_| VfsError::NotFound)?;
    let mut item = BtrfsInodeItem::decode(&planner.tree_items(tree).map_err(vfs)?[index].value)
        .map_err(vfs)?;
    item.nlink = item.nlink.checked_sub(1).ok_or(VfsError::Io)?;
    if item.nlink == 0 {
        let _ = planner.delete_item(tree, key).map_err(vfs)?;
    } else {
        planner.set_item(tree, key, item.encode()).map_err(vfs)?;
    }
    Ok(())
}

fn kind_from_dir_type(value: u8) -> NodeType {
    match value {
        1 => NodeType::RegularFile,
        2 => NodeType::Directory,
        3 => NodeType::CharacterDevice,
        4 => NodeType::BlockDevice,
        5 => NodeType::Fifo,
        6 => NodeType::Socket,
        7 => NodeType::Symlink,
        _ => NodeType::Unknown,
    }
}
fn dir_type(value: NodeType) -> u8 {
    match value {
        NodeType::RegularFile => 1,
        NodeType::Directory => 2,
        NodeType::CharacterDevice => 3,
        NodeType::BlockDevice => 4,
        NodeType::Fifo => 5,
        NodeType::Socket => 6,
        NodeType::Symlink => 7,
        NodeType::Unknown => 0,
    }
}
fn vfs(error: AxError) -> VfsError {
    error
}
