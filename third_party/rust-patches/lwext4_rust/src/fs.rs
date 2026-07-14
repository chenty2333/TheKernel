use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{marker::PhantomData, mem, sync::atomic::AtomicU64, time::Duration};

use hashbrown::HashMap;

use crate::{
    DirLookupResult, DirReader, Ext4Error, Ext4Result, FileAttr, InodeRef, InodeToken, InodeType,
    blockdev::{
        AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, EXT4_DEV_BSIZE, Ext4BlockDevice,
    },
    error::Context,
    ffi::*,
    hot::{
        ENABLE_HOT_INODE_CACHE, HotInodeCache, async_mapped_read_enabled, record_async_mapped_read,
        record_async_mapped_read_cookie_reject, record_async_mapped_read_fallback,
        record_hot_inode_hit, record_hot_inode_miss, record_inode_ref_get,
        record_mapped_overwrite_vectored_hit, record_mapped_read, record_mapped_read_vectored,
    },
    iomap::{MappedRun, MappedRunKind},
    util::get_block_size,
};

pub trait SystemHal {
    fn now() -> Option<Duration>;
}

pub struct DummyHal;
impl SystemHal for DummyHal {
    fn now() -> Option<Duration> {
        None
    }
}

#[derive(Debug, Clone)]
pub struct FsConfig {
    pub bcache_size: u32,
}
impl Default for FsConfig {
    fn default() -> Self {
        Self {
            bcache_size: CONFIG_BLOCK_DEV_CACHE_SIZE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatFs {
    pub inodes_count: u32,
    pub free_inodes_count: u32,

    pub blocks_count: u64,
    pub free_blocks_count: u64,
    pub block_size: u32,
}

fn inode_needs_truncate_on_unlink(ty: InodeType) -> bool {
    matches!(
        ty,
        InodeType::RegularFile | InodeType::Directory | InodeType::Symlink
    )
}

fn apply_hardlink_timestamps<Hal: SystemHal>(
    parent: &mut InodeRef<Hal>,
    source: &mut InodeRef<Hal>,
    now: &Duration,
) {
    source.set_ctime(now);
    parent.set_mtime(now);
    parent.set_ctime(now);
}

fn apply_unlink_timestamps<Hal: SystemHal>(
    parent: &mut InodeRef<Hal>,
    victim: &mut InodeRef<Hal>,
    now: &Duration,
) {
    victim.set_ctime(now);
    parent.set_mtime(now);
    parent.set_ctime(now);
}

fn apply_rename_timestamps<Hal: SystemHal>(
    source: &mut InodeRef<Hal>,
    replaced: Option<&mut InodeRef<Hal>>,
    old_parent: &mut InodeRef<Hal>,
    new_parent: Option<&mut InodeRef<Hal>>,
    now: &Duration,
) {
    source.set_ctime(now);
    if let Some(replaced) = replaced {
        replaced.set_ctime(now);
    }
    old_parent.set_mtime(now);
    old_parent.set_ctime(now);
    if let Some(new_parent) = new_parent {
        new_parent.set_mtime(now);
        new_parent.set_ctime(now);
    }
}

fn rename_requires_destination_parent_link_growth(
    source_type: InodeType,
    source_parent: u32,
    destination_parent: u32,
    destination_type: Option<InodeType>,
) -> bool {
    source_type == InodeType::Directory
        && source_parent != destination_parent
        && destination_type != Some(InodeType::Directory)
}

fn runs_align_segments(runs: &[MappedRun], segments: impl IntoIterator<Item = usize>) -> bool {
    let mut run_index = 0usize;
    let mut run_remaining = runs.first().map(|run| run.bytes).unwrap_or(0);
    for segment_len in segments {
        if segment_len == 0 {
            continue;
        }
        if run_index >= runs.len() || segment_len > run_remaining {
            return false;
        }
        run_remaining -= segment_len;
        if run_remaining == 0 {
            run_index += 1;
            run_remaining = runs.get(run_index).map(|run| run.bytes).unwrap_or(0);
        }
    }
    run_index == runs.len() && run_remaining == 0
}

fn segments_are_device_block_sized(segments: impl IntoIterator<Item = usize>) -> bool {
    segments
        .into_iter()
        .all(|len| len == 0 || len % EXT4_DEV_BSIZE == 0)
}

fn fail_closed_after_started_mutation<T>(
    metadata_poisoned: &mut bool,
    mutation_started: bool,
    result: Ext4Result<T>,
) -> Ext4Result<T> {
    if mutation_started && result.is_err() {
        *metadata_poisoned = true;
    }
    result
}

fn finish_committed_namespace_step(
    metadata_poisoned: &mut bool,
    context: &'static str,
    result: Ext4Result<()>,
) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            // The namespace decision is already externally visible. Reporting
            // an ordinary operation error would invite an unsafe retry while
            // an outer transaction can only release private reservations.
            // Preserve the committed syscall outcome, but stop every later
            // metadata mutation and make the writeback failure observable.
            *metadata_poisoned = true;
            log::error!("{context} failed after ext4 namespace commit: {error}");
            false
        }
    }
}

fn complete_namespace_operation(
    metadata_poisoned: &mut bool,
    mutation_started: bool,
    namespace_committed: bool,
    committed_context: &'static str,
    result: Ext4Result<()>,
) -> Ext4Result<()> {
    if namespace_committed {
        finish_committed_namespace_step(metadata_poisoned, committed_context, result);
        Ok(())
    } else {
        fail_closed_after_started_mutation(metadata_poisoned, mutation_started, result)
    }
}

pub struct Ext4Filesystem<Hal: SystemHal, Dev: BlockDevice> {
    inner: Box<ext4_fs>,
    bdev: Ext4BlockDevice<Dev>,
    hot_inodes: HotInodeCache<Hal>,
    inode_handles: HashMap<InodeToken, InodeHandleState>,
    metadata_poisoned: bool,
    shutdown_state: ShutdownState,
    _phantom: PhantomData<Hal>,
}

#[derive(Clone, Copy)]
struct ShutdownFailure {
    code: i32,
    context: Option<&'static str>,
    metadata_may_have_changed: bool,
}

impl ShutdownFailure {
    fn from_error(error: &Ext4Error) -> Self {
        Self {
            code: error.code,
            context: error.context,
            metadata_may_have_changed: error.metadata_may_have_changed(),
        }
    }

    fn into_error(self) -> Ext4Error {
        Ext4Error::new(self.code, self.context)
            .with_metadata_may_have_changed(self.metadata_may_have_changed)
    }
}

#[derive(Clone, Copy)]
enum ShutdownState {
    Active,
    Complete(Option<ShutdownFailure>),
}

impl ShutdownState {
    fn completed_result(self) -> Option<Ext4Result<()>> {
        match self {
            Self::Active => None,
            Self::Complete(None) => Some(Ok(())),
            Self::Complete(Some(failure)) => Some(Err(failure.into_error())),
        }
    }
}

struct InodeHandleState {
    handles: usize,
    pending_delete: Option<PendingDelete>,
    namespace_epoch: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingDelete {
    Ready(InodeType),
    Finalizing(InodeType),
    Failed,
}

enum FinalizeUnlinked {
    /// No inode-allocation state was changed; a later reap may retry.
    Retryable(Ext4Error),
    /// `ext4_fs_free_inode` reported an error after it may have changed
    /// allocation metadata. Retrying could double-free blocks or the inode.
    Failed(Ext4Error),
    /// The inode allocation was released. The contained result is the final
    /// inode-reference put, which may still report a writeback error.
    Committed(Ext4Result<()>),
}

#[derive(Debug, Eq, PartialEq)]
enum HandleRelease {
    Retained,
    Removed,
    Untracked,
    Underflow,
}

fn release_inode_handle_state(
    handles: &mut HashMap<InodeToken, InodeHandleState>,
    token: InodeToken,
) -> HandleRelease {
    let Some(state) = handles.get_mut(&token) else {
        return HandleRelease::Untracked;
    };
    if state.handles == 0 {
        return HandleRelease::Underflow;
    }
    state.handles -= 1;
    if state.handles == 0 && state.pending_delete.is_none() {
        handles.remove(&token);
        HandleRelease::Removed
    } else {
        HandleRelease::Retained
    }
}

const NAMESPACE_REAP_BUDGET: usize = 32;
/// Hard bound for long-lived VFS inode identities and deferred deletions.
/// This also bounds final filesystem teardown work.
const MAX_TRACKED_INODE_IDENTITIES: usize = 16_384;

fn try_namespace_epoch() -> Ext4Result<Arc<AtomicU64>> {
    Arc::try_new(AtomicU64::new(0))
        .map_err(|_| Ext4Error::new(ENOMEM as _, "ext4 inode identity allocation failed"))
}

fn combine_operation_and_release<T>(
    operation: Ext4Result<T>,
    release: Ext4Result<()>,
) -> Ext4Result<T> {
    match (operation, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(release_err)) => {
            log::error!("secondary ext4 inode-release failure: {release_err}");
            Err(err.with_metadata_may_have_changed(release_err.metadata_may_have_changed()))
        }
    }
}

fn preserve_operation_error(primary: Ext4Error, release: Ext4Result<()>) -> Ext4Error {
    match release {
        Ok(()) => primary,
        Err(release_err) => {
            log::error!("secondary ext4 inode-release failure: {release_err}");
            primary.with_metadata_may_have_changed(release_err.metadata_may_have_changed())
        }
    }
}

fn record_shutdown_step(first_error: &mut Option<Ext4Error>, result: Ext4Result<()>) {
    let Err(error) = result else {
        return;
    };
    if let Some(primary) = first_error.take() {
        log::error!("secondary ext4 shutdown failure: {error}");
        *first_error =
            Some(primary.with_metadata_may_have_changed(error.metadata_may_have_changed()));
    } else {
        *first_error = Some(error);
    }
}

impl<Hal: SystemHal, Dev: BlockDevice> Ext4Filesystem<Hal, Dev> {
    pub fn new(dev: Dev, config: FsConfig) -> Ext4Result<Self> {
        let mut bdev = Ext4BlockDevice::new(dev)?;
        let hot_inodes = HotInodeCache::try_new()?;
        let mut fs = Box::try_new(unsafe { mem::zeroed() })
            .map_err(|_| Ext4Error::new(ENOMEM as _, "ext4 filesystem allocation failed"))?;
        unsafe {
            let bd = bdev.inner.as_mut();
            ext4_fs_init(&mut *fs, bd, false).context("ext4_fs_init")?;

            let bs = get_block_size(&fs.sb);
            ext4_block_set_lb_size(bd, bs);
            ext4_bcache_init_dynamic(bd.bc, config.bcache_size, bs)
                .context("ext4_bcache_init_dynamic")?;
            if bs != (*bd.bc).itemsize {
                return Err(Ext4Error::new(ENOTSUP as _, "block size mismatch"));
            }

            bd.fs = &mut *fs;

            let mut result = Self {
                inner: fs,
                bdev,
                hot_inodes,
                inode_handles: HashMap::new(),
                metadata_poisoned: false,
                shutdown_state: ShutdownState::Active,
                _phantom: PhantomData,
            };
            let bd = result.bdev.inner.as_mut();
            ext4_block_bind_bcache(bd, bd.bc).context("ext4_block_bind_bcache")?;
            Ok(result)
        }
    }

    fn ensure_active(&self) -> Ext4Result<()> {
        match self.shutdown_state {
            ShutdownState::Active => Ok(()),
            ShutdownState::Complete(_) => Err(Ext4Error::new(
                EIO as _,
                "ext4 filesystem has already been shut down",
            )),
        }
    }

    fn inode_ref(&mut self, ino: u32) -> Ext4Result<InodeRef<Hal>> {
        self.ensure_active()?;
        let mut inode = InodeRef::try_uninitialized()?;
        record_inode_ref_get();
        unsafe {
            ext4_fs_get_inode_ref(self.inner.as_mut(), ino, inode.inner.as_mut())
                .context("ext4_fs_get_inode_ref")?;
            inode.activate();
            Ok(inode)
        }
    }

    fn ensure_metadata_writable(&self) -> Ext4Result<()> {
        self.ensure_active()?;
        if self.metadata_poisoned {
            Err(Ext4Error::new(
                EIO as _,
                "ext4 metadata state is poisoned after a potentially partial mutation",
            ))
        } else {
            Ok(())
        }
    }

    /// Stop all later metadata mutations after an upper layer observes an
    /// operation whose committed namespace state cannot be reconstructed.
    pub fn mark_metadata_poisoned(&mut self) {
        self.metadata_poisoned = true;
    }

    fn observe_metadata_result<T>(&mut self, result: Ext4Result<T>) -> Ext4Result<T> {
        if result
            .as_ref()
            .is_err_and(Ext4Error::metadata_may_have_changed)
        {
            self.metadata_poisoned = true;
        }
        result
    }

    fn with_cached_inode_ref<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&mut InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        self.ensure_active()?;
        if !ENABLE_HOT_INODE_CACHE.load(core::sync::atomic::Ordering::Relaxed) {
            let mut inode = self.inode_ref(ino)?;
            let result = f(&mut inode);
            let result = combine_operation_and_release(result, inode.finish());
            return self.observe_metadata_result(result);
        }

        let mut inode = if let Some(inode) = self.hot_inodes.take(ino) {
            record_hot_inode_hit();
            inode
        } else {
            record_hot_inode_miss();
            self.inode_ref(ino)?
        };
        let result = f(&mut inode);
        let release = match self.hot_inodes.put(ino, inode) {
            Some(evicted) => evicted.finish(),
            None => Ok(()),
        };
        let result = combine_operation_and_release(result, release);
        self.observe_metadata_result(result)
    }

    fn invalidate_hot_inode(&mut self, ino: u32) -> Ext4Result<()> {
        let result = match self.hot_inodes.invalidate(ino) {
            Some(inode) => inode.finish(),
            None => Ok(()),
        };
        self.observe_metadata_result(result)
    }

    fn drain_hot_inodes(&mut self) -> Ext4Result<()> {
        let mut first_error = None;
        for inode in self.hot_inodes.drain_all() {
            if let Err(err) = inode.finish()
                && first_error.is_none()
            {
                first_error = Some(err);
            }
        }
        let result = match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        };
        self.observe_metadata_result(result)
    }

    fn clone_ref(&mut self, inode: &InodeRef<Hal>) -> Ext4Result<InodeRef<Hal>> {
        self.inode_ref(inode.ino())
    }

    fn apply_committed_rename_timestamps(
        &mut self,
        source: &mut InodeRef<Hal>,
        replaced: Option<InodeToken>,
        old_parent: &mut InodeRef<Hal>,
        new_parent: Option<&mut InodeRef<Hal>>,
        timestamp: Option<Duration>,
    ) -> Ext4Result<()> {
        let Some(now) = timestamp else {
            return Ok(());
        };
        let mut replaced_ref = match replaced {
            Some(expected) => {
                // RenameRequest keeps the exact replaced VFS entry retained
                // through this backend call, so unlink cannot reap the inode
                // before its committed ctime update.
                let actual = self.inode_ref(expected.ino())?;
                if actual.token() != expected {
                    let error = Ext4Error::new(
                        EIO as _,
                        "stale ext4 rename victim during timestamp commit",
                    );
                    return combine_operation_and_release(Err(error), actual.finish());
                }
                Some(actual)
            }
            None => None,
        };
        apply_rename_timestamps(source, replaced_ref.as_mut(), old_parent, new_parent, &now);
        match replaced_ref {
            Some(replaced) => replaced.finish(),
            None => Ok(()),
        }
    }

    pub fn lookup_inode(&mut self, parent: u32, name: &str) -> Ext4Result<(u32, InodeType)> {
        let mut result = self.lookup(parent, name)?;
        let entry = result.entry()?;
        let identity = (entry.ino(), entry.inode_type());
        self.observe_metadata_result(result.finish())?;
        Ok(identity)
    }

    pub fn with_inode_ref<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        let inode = self.inode_ref(ino)?;
        let result = f(&inode);
        let release = inode.finish();
        combine_operation_and_release(result, release)
    }

    pub fn with_inode_ref_mut<R>(
        &mut self,
        ino: u32,
        f: impl FnOnce(&mut InodeRef<Hal>) -> Ext4Result<R>,
    ) -> Ext4Result<R> {
        self.ensure_metadata_writable()?;
        let mut inode = self.inode_ref(ino)?;
        let result = f(&mut inode);
        let release = inode.finish();
        let result = combine_operation_and_release(result, release);
        self.observe_metadata_result(result)
    }

    pub fn retain_inode_handle(&mut self, ino: u32) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        // Validate the inode before publishing a long-lived VFS node.
        let inode = self.inode_ref(ino)?;
        let token = inode.token();
        if inode.nlink() == 0 {
            let err = Ext4Error::new(ENOENT as _, "cannot retain an unlinked inode");
            return combine_operation_and_release(Err(err), inode.finish());
        }
        inode.finish()?;
        if self
            .inode_handles
            .keys()
            .any(|tracked| tracked.ino() == ino && *tracked != token)
        {
            return Err(Ext4Error::new(
                EIO as _,
                "conflicting retained ext4 inode generation",
            ));
        }
        if let Some(state) = self.inode_handles.get_mut(&token) {
            state.handles = state
                .handles
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ENOMEM as _, "inode handle count overflow"))?;
            return Ok((token, state.namespace_epoch.clone()));
        }
        {
            if self.inode_handles.len() >= MAX_TRACKED_INODE_IDENTITIES {
                return Err(Ext4Error::new(
                    ENOMEM as _,
                    "ext4 retained inode identity limit reached",
                ));
            }
            self.inode_handles.try_reserve(1).map_err(|_| {
                Ext4Error::new(ENOMEM as _, "ext4 inode identity allocation failed")
            })?;
        }
        let namespace_epoch = try_namespace_epoch()?;
        self.inode_handles.insert(
            token,
            InodeHandleState {
                handles: 1,
                pending_delete: None,
                namespace_epoch: namespace_epoch.clone(),
            },
        );
        Ok((token, namespace_epoch))
    }

    pub fn release_inode_handle(&mut self, token: InodeToken) {
        match release_inode_handle_state(&mut self.inode_handles, token) {
            HandleRelease::Retained | HandleRelease::Removed => {}
            HandleRelease::Untracked => {
                log::error!("release of untracked ext4 inode handle {token:?}");
            }
            HandleRelease::Underflow => {
                log::error!("ext4 inode handle underflow for {token:?}");
            }
        }
    }

    fn finalize_unlinked_inode(
        &mut self,
        token: InodeToken,
        inode_type: InodeType,
    ) -> FinalizeUnlinked {
        if let Err(err) = self.invalidate_hot_inode(token.ino()) {
            return FinalizeUnlinked::Retryable(err);
        }
        let mut inode = match self.inode_ref(token.ino()) {
            Ok(inode) => inode,
            Err(err) => return FinalizeUnlinked::Retryable(err),
        };
        if inode.token() != token {
            let err = Ext4Error::new(EIO as _, "stale ext4 inode token during final delete");
            return FinalizeUnlinked::Failed(preserve_operation_error(err, inode.finish()));
        }
        if inode_needs_truncate_on_unlink(inode_type)
            && let Err(err) = inode.truncate(0)
        {
            // Truncate updates block pointers and allocation metadata a piece
            // at a time. An error is not a proven rollback point, so fail the
            // deletion state closed instead of retrying a potentially partial
            // truncate automatically.
            return FinalizeUnlinked::Failed(preserve_operation_error(err, inode.finish()));
        }
        let free_result = unsafe {
            ext4_inode_set_del_time(inode.inner.inode, u32::MAX);
            inode.mark_dirty();
            ext4_fs_free_inode(inode.inner.as_mut()).context("ext4_fs_free_inode")
        };
        match free_result {
            Ok(()) => FinalizeUnlinked::Committed(inode.finish()),
            Err(err) => {
                let err = preserve_operation_error(err, inode.finish());
                FinalizeUnlinked::Failed(err)
            }
        }
    }

    fn set_pending_delete_state(
        &mut self,
        token: InodeToken,
        pending_delete: PendingDelete,
    ) -> Ext4Result<()> {
        let Some(state) = self.inode_handles.get_mut(&token) else {
            self.metadata_poisoned = true;
            return Err(Ext4Error::new(
                EIO as _,
                "ext4 inode finalization lost its tracked identity",
            ));
        };
        state.pending_delete = Some(pending_delete);
        Ok(())
    }

    fn reap_pending_unlinked_inodes(&mut self, budget: usize) -> Ext4Result<usize> {
        if self.metadata_poisoned {
            return Err(Ext4Error::new(
                EIO as _,
                "refusing ext4 inode finalization after metadata poison",
            ));
        }
        if self.inode_handles.values().any(|state| {
            matches!(
                state.pending_delete,
                Some(PendingDelete::Failed | PendingDelete::Finalizing(_))
            )
        }) {
            return Err(Ext4Error::new(
                EIO as _,
                "ext4 inode finalization is in a failed state",
            ));
        }
        let mut reaped = 0;
        while reaped < budget {
            let Some((token, inode_type)) =
                self.inode_handles.iter().find_map(|(&token, state)| {
                    (state.handles == 0).then_some(match state.pending_delete? {
                        PendingDelete::Ready(inode_type) => Some((token, inode_type)),
                        PendingDelete::Finalizing(_) | PendingDelete::Failed => None,
                    })?
                })
            else {
                break;
            };
            self.set_pending_delete_state(token, PendingDelete::Finalizing(inode_type))?;
            match self.finalize_unlinked_inode(token, inode_type) {
                FinalizeUnlinked::Retryable(err) => {
                    self.set_pending_delete_state(token, PendingDelete::Ready(inode_type))?;
                    return Err(err);
                }
                FinalizeUnlinked::Failed(err) => {
                    self.metadata_poisoned = true;
                    self.set_pending_delete_state(token, PendingDelete::Failed)?;
                    return Err(err);
                }
                FinalizeUnlinked::Committed(result) => {
                    if self.inode_handles.remove(&token).is_none() {
                        self.metadata_poisoned = true;
                        return Err(Ext4Error::new(
                            EIO as _,
                            "ext4 inode finalization lost its committed identity",
                        ));
                    }
                    reaped += 1;
                    self.observe_metadata_result(result)?;
                }
            }
        }
        Ok(reaped)
    }

    fn mapped_aligned_read_plan(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
    ) -> Ext4Result<Option<(usize, u32, Vec<MappedRun>)>> {
        self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            if offset >= file_size {
                return Ok(Some((0, block_size, Vec::new())));
            }
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
            {
                return Ok(None);
            }
            let to_be_read = len.min((file_size - offset) as usize);
            if to_be_read != len || to_be_read % block_size as usize != 0 {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, to_be_read, false)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((to_be_read, block_size, runs)))
        })
    }

    fn mapped_runs_current(&mut self, ino: u32, runs: &[MappedRun]) -> Ext4Result<bool> {
        self.with_cached_inode_ref(ino, |inode| {
            let mut expected_offset = runs.first().map(|run| run.file_offset).unwrap_or(0);
            for run in runs {
                if run.seq != inode.mapping_seq || run.file_offset != expected_offset {
                    return Ok(false);
                }
                expected_offset = run.end_offset();
            }
            Ok(true)
        })
    }

    fn try_read_mapped_runs_async(
        &mut self,
        ino: u32,
        runs: &[MappedRun],
        block_size: u32,
        bufs: &mut [&mut [u8]],
        bytes: usize,
    ) -> Ext4Result<Option<usize>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        if runs.is_empty() {
            return Ok(Some(0));
        }
        let block_size = block_size as usize;
        if block_size == 0
            || bufs
                .iter()
                .any(|buf| !buf.is_empty() && buf.len() % block_size != 0)
        {
            return Ok(None);
        }
        if !self.mapped_runs_current(ino, runs)? {
            record_async_mapped_read_cookie_reject();
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submit_batches = 0usize;
        for run in runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                if segment >= bufs.len() {
                    return Ok(None);
                }
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                if segment_len > remaining {
                    return Ok(None);
                }
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(stats) = self
                .bdev
                .dev_mut()
                .try_read_blocks_vectored_async(block_id, &mut bufs[start..segment])?
            else {
                return Ok(None);
            };
            submit_batches += stats.submit_batches;
        }

        record_async_mapped_read(runs.len(), bytes, submit_batches);
        Ok(Some(bytes))
    }

    fn try_read_at_aligned_hot_async(
        &mut self,
        ino: u32,
        buf: &mut [u8],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, buf.len())?
        else {
            record_async_mapped_read_fallback();
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(0));
        }
        if !runs_align_segments(&runs, [to_be_read]) {
            record_async_mapped_read_fallback();
            return Ok(None);
        }

        let read_buf = &mut buf[..to_be_read];
        let mut bufs = [read_buf];
        let Some(read) =
            self.try_read_mapped_runs_async(ino, &runs, block_size, &mut bufs, to_be_read)?
        else {
            record_async_mapped_read_fallback();
            return Ok(None);
        };
        record_mapped_read(runs.len(), read);
        Ok(Some(read))
    }

    fn try_read_mapped_runs_async_submit(
        &mut self,
        ino: u32,
        runs: &[MappedRun],
        block_size: u32,
        bufs: &mut [&mut [u8]],
        bytes: usize,
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        if !async_mapped_read_enabled() {
            return Ok(None);
        }
        if runs.is_empty() {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        let block_size = block_size as usize;
        if block_size == 0
            || bufs
                .iter()
                .any(|buf| !buf.is_empty() && buf.len() % block_size != 0)
        {
            return Ok(None);
        }
        if !self.mapped_runs_current(ino, runs)? {
            record_async_mapped_read_cookie_reject();
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submission = AsyncReadSubmission {
            bytes,
            ..AsyncReadSubmission::default()
        };
        for run in runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                if segment >= bufs.len() {
                    return Ok(None);
                }
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                if segment_len > remaining {
                    return Ok(None);
                }
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(run_submission) = self
                .bdev
                .dev_mut()
                .try_read_blocks_vectored_async_submit(block_id, &mut bufs[start..segment])?
            else {
                return Ok(None);
            };
            submission.submit_batches += run_submission.submit_batches;
            submission.handles.extend(run_submission.handles);
        }

        record_async_mapped_read(runs.len(), bytes, submission.submit_batches);
        Ok(Some(submission))
    }

    pub(crate) fn alloc_inode(&mut self, ty: InodeType) -> Ext4Result<InodeRef<Hal>> {
        self.ensure_metadata_writable()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        let mut result = InodeRef::try_uninitialized()?;
        unsafe {
            let ty = match ty {
                InodeType::Fifo => EXT4_DE_FIFO,
                InodeType::CharacterDevice => EXT4_DE_CHRDEV,
                InodeType::Directory => EXT4_DE_DIR,
                InodeType::BlockDevice => EXT4_DE_BLKDEV,
                InodeType::RegularFile => EXT4_DE_REG_FILE,
                InodeType::Symlink => EXT4_DE_SYMLINK,
                InodeType::Socket => EXT4_DE_SOCK,
                InodeType::Unknown => EXT4_DE_UNKNOWN,
            };
            let mut metadata_may_have_changed = false;
            let allocation = ext4_fs_alloc_inode_status(
                self.inner.as_mut(),
                result.inner.as_mut(),
                ty as _,
                &mut metadata_may_have_changed,
            )
            .context("ext4_fs_alloc_inode")
            .map_err(|error| error.with_metadata_may_have_changed(metadata_may_have_changed));
            self.observe_metadata_result(allocation)?;
            result.activate();
            ext4_fs_inode_blocks_init(self.inner.as_mut(), result.inner.as_mut());
            Ok(result)
        }
    }

    pub fn get_attr(&mut self, ino: u32, attr: &mut FileAttr) -> Ext4Result<()> {
        self.with_inode_ref(ino, |inode| {
            inode.get_attr(attr);
            Ok(())
        })
    }

    pub fn read_at(&mut self, ino: u32, buf: &mut [u8], offset: u64) -> Ext4Result<usize> {
        self.with_cached_inode_ref(ino, |inode| inode.read_at(buf, offset))
    }
    pub fn read_at_aligned_hot(
        &mut self,
        ino: u32,
        buf: &mut [u8],
        offset: u64,
    ) -> Ext4Result<usize> {
        if let Some(read) = self.try_read_at_aligned_hot_async(ino, buf, offset)? {
            return Ok(read);
        }
        self.with_cached_inode_ref(ino, |inode| inode.read_at_aligned_hot(buf, offset))
    }
    pub fn read_at_aligned_hot_vectored(
        &mut self,
        ino: u32,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(0));
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, len)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(0));
        }
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }

        if let Some(read) =
            self.try_read_mapped_runs_async(ino, &runs, block_size, bufs, to_be_read)?
        {
            record_mapped_read_vectored(runs.len(), read);
            return Ok(Some(read));
        } else if async_mapped_read_enabled() {
            record_async_mapped_read_fallback();
        }

        let mut segment = 0usize;
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            self.bdev
                .dev_mut()
                .read_blocks_vectored(block_id, &mut bufs[start..segment])?;
        }
        record_mapped_read_vectored(runs.len(), to_be_read);
        Ok(Some(to_be_read))
    }

    pub fn read_at_aligned_hot_vectored_async_submit(
        &mut self,
        ino: u32,
        bufs: &mut [&mut [u8]],
        offset: u64,
    ) -> Ext4Result<Option<AsyncReadSubmission>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        let Some((to_be_read, block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, len)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(AsyncReadSubmission::default()));
        }
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        }

        let Some(submission) =
            self.try_read_mapped_runs_async_submit(ino, &runs, block_size, bufs, to_be_read)?
        else {
            if async_mapped_read_enabled() {
                record_async_mapped_read_fallback();
            }
            return Ok(None);
        };
        record_mapped_read_vectored(runs.len(), to_be_read);
        Ok(Some(submission))
    }
    pub fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.ensure_metadata_writable()?;
        self.with_cached_inode_ref(ino, |inode| inode.write_at(buf, offset))
    }
    pub fn write_at_aligned_hot(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.ensure_metadata_writable()?;
        self.with_cached_inode_ref(ino, |inode| inode.write_at_aligned_hot(buf, offset))
    }
    pub fn write_at_aligned_hot_vectored(
        &mut self,
        ino: u32,
        bufs: &[&[u8]],
        offset: u64,
    ) -> Ext4Result<Option<usize>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(0));
        }
        self.ensure_metadata_writable()?;
        let Some((block_size, runs)) = self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            let Some(end) = offset.checked_add(len as u64) else {
                return Ok(None);
            };
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
                || end > file_size
            {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, len, true)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((block_size, runs)))
        })?
        else {
            return Ok(None);
        };
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }

        let mut segment = 0usize;
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            self.bdev
                .dev_mut()
                .write_blocks_vectored(block_id, &bufs[start..segment])?;
            self.bdev.invalidate_logical_block_range(
                run.pblock,
                (run.bytes / block_size as usize) as u32,
            );
        }
        record_mapped_overwrite_vectored_hit(len);
        Ok(Some(len))
    }

    pub fn write_at_aligned_hot_vectored_async_submit(
        &mut self,
        ino: u32,
        bufs: &[&[u8]],
        offset: u64,
    ) -> Ext4Result<Option<AsyncWriteSubmission>> {
        let len = bufs.iter().map(|buf| buf.len()).sum::<usize>();
        if len == 0 {
            return Ok(Some(AsyncWriteSubmission::default()));
        }
        self.ensure_metadata_writable()?;
        let Some((block_size, runs)) = self.with_cached_inode_ref(ino, |inode| {
            let file_size = inode.size();
            let block_size = get_block_size(inode.superblock());
            let Some(end) = offset.checked_add(len as u64) else {
                return Ok(None);
            };
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() == InodeType::Symlink
                || end > file_size
            {
                return Ok(None);
            }
            let Some(runs) = inode.map_iomap_runs(offset, len, true)? else {
                return Ok(None);
            };
            if runs
                .iter()
                .any(|run| run.kind != MappedRunKind::Written || run.pblock == 0)
            {
                return Ok(None);
            }
            Ok(Some((block_size, runs)))
        })?
        else {
            return Ok(None);
        };
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut submission = AsyncWriteSubmission {
            bytes: len,
            ..AsyncWriteSubmission::default()
        };
        for run in &runs {
            let start = segment;
            let mut remaining = run.bytes;
            while remaining > 0 {
                let segment_len = bufs[segment].len();
                segment += 1;
                if segment_len == 0 {
                    continue;
                }
                debug_assert!(segment_len <= remaining);
                remaining -= segment_len;
            }
            let block_id = self.bdev.direct_physical_block_id(run.pblock);
            let Some(run_submission) = self
                .bdev
                .dev_mut()
                .try_write_blocks_vectored_async_submit(block_id, &bufs[start..segment])?
            else {
                return Ok(None);
            };
            submission.submit_batches += run_submission.submit_batches;
            submission.handles.extend(run_submission.handles);
            self.bdev.invalidate_logical_block_range(
                run.pblock,
                (run.bytes / block_size as usize) as u32,
            );
        }
        record_mapped_overwrite_vectored_hit(len);
        Ok(Some(submission))
    }
    pub fn is_block_aligned_range(&self, offset: u64, len: usize) -> bool {
        let block_size = get_block_size(&self.inner.as_ref().sb) as u64;
        block_size == 4096 && offset % block_size == 0 && len as u64 % block_size == 0
    }
    pub fn set_len(&mut self, ino: u32, len: u64) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let shrinking = len < inode.size();
        let operation = inode.set_len(len);
        if shrinking && operation.is_err() {
            // ext4_fs_truncate_inode updates allocation metadata incrementally.
            // Once it reports an error, retrying later mutations could build on
            // a partially truncated inode or double-release blocks.
            self.metadata_poisoned = true;
        }
        let operation = combine_operation_and_release(operation, inode.finish());
        let invalidate = self.invalidate_hot_inode(ino);
        let operation = combine_operation_and_release(operation, invalidate);
        self.observe_metadata_result(operation)
    }
    pub fn set_symlink(&mut self, _ino: u32, _buf: &[u8]) -> Ext4Result<()> {
        Err(Ext4Error::new(
            ENOTSUP as _,
            "rewriting an ext4 symbolic link is not supported",
        ))
    }
    pub fn lookup(&mut self, parent: u32, name: &str) -> Ext4Result<DirLookupResult<Hal>> {
        let result = self.inode_ref(parent)?.lookup(name);
        self.observe_metadata_result(result)
    }
    pub fn read_dir(&mut self, parent: u32, offset: u64) -> Ext4Result<DirReader<Hal>> {
        self.inode_ref(parent)?.read_dir(offset)
    }

    fn discard_unpublished_inode(&mut self, mut inode: InodeRef<Hal>) -> Ext4Result<()> {
        if inode_needs_truncate_on_unlink(inode.inode_type())
            && let Err(err) = inode.truncate(0)
        {
            self.metadata_poisoned = true;
            return combine_operation_and_release(Err(err), inode.finish());
        }

        let free =
            unsafe { ext4_fs_free_inode(inode.inner.as_mut()) }.context("ext4_fs_free_inode");
        if free.is_ok() {
            // Match lwext4's failed-create cleanup: the released inode slot
            // must not be written back as a newly initialized live inode.
            inode.inner.dirty = false;
        } else {
            self.metadata_poisoned = true;
        }
        combine_operation_and_release(free, inode.finish())
    }

    fn create_inner(
        &mut self,
        parent_ino: u32,
        name: &str,
        ty: InodeType,
        mode: u32,
        symlink_target: Option<&[u8]>,
        user: Option<(u32, u32)>,
        rdev: Option<u64>,
        initial_time: Option<Duration>,
    ) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        if rdev.is_some() && !matches!(ty, InodeType::CharacterDevice | InodeType::BlockDevice) {
            return Err(Ext4Error::new(
                EINVAL as _,
                "device identity for non-device inode",
            ));
        }
        if rdev.is_some_and(|rdev| u32::try_from(rdev).is_err()) {
            return Err(Ext4Error::new(
                EINVAL as _,
                "ext4 device identity exceeds on-disk encoding",
            ));
        }
        self.ensure_metadata_writable()?;
        self.drain_hot_inodes()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        if self.inode_handles.len() >= MAX_TRACKED_INODE_IDENTITIES {
            return Err(Ext4Error::new(
                ENOMEM as _,
                "ext4 retained inode identity limit reached",
            ));
        }
        self.inode_handles
            .try_reserve(1)
            .map_err(|_| Ext4Error::new(ENOMEM as _, "ext4 inode identity allocation failed"))?;
        // Identity ownership must be admitted before allocating an on-disk
        // inode.  From `alloc_inode` onward cleanup may itself require I/O and
        // cannot make an allocation failure look like a transaction abort.
        let namespace_epoch = try_namespace_epoch()?;
        let mut child = self.alloc_inode(ty)?;
        let token = child.token();
        if self
            .inode_handles
            .keys()
            .any(|tracked| tracked.ino() == token.ino())
        {
            self.metadata_poisoned = true;
            let primary = Ext4Error::new(EIO as _, "ext4 allocator reused a retained inode slot");
            // Do not truncate or free an inode number still owned by a live
            // identity. Release only this C cache reference and leave repair to
            // the poisoned-filesystem teardown path.
            child.inner.dirty = false;
            return combine_operation_and_release(Err(primary), child.finish());
        }
        self.inode_handles.insert(
            token,
            InodeHandleState {
                handles: 1,
                pending_delete: None,
                namespace_epoch: namespace_epoch.clone(),
            },
        );
        let mut parent = match self.inode_ref(parent_ino) {
            Ok(parent) => parent,
            Err(primary) => {
                self.release_inode_handle(token);
                let cleanup = self.discard_unpublished_inode(child);
                let cleanup = self.observe_metadata_result(cleanup);
                if let Err(cleanup) = cleanup {
                    log::error!("ext4 create cleanup failed after parent lookup error: {cleanup}");
                }
                return Err(primary);
            }
        };
        let mut parent_link_added = false;

        let prepare = (|| {
            if let Some((uid, gid)) = user {
                child.set_owner(uid, gid);
            }
            if let Some(rdev) = rdev {
                child.set_rdev(rdev);
            }
            if let Some(target) = symlink_target {
                if ty != InodeType::Symlink {
                    return Err(Ext4Error::new(
                        EINVAL as _,
                        "symlink target for non-symlink inode",
                    ));
                }
                child.initialize_symlink(target)?;
            } else if ty == InodeType::Directory {
                let mut self_ref = self.clone_ref(&child)?;
                let dot = child.add_entry(".", &mut self_ref);
                combine_operation_and_release(dot, self_ref.finish())?;
                child.add_entry("..", &mut parent)?;
                parent_link_added = true;

                // Before publication, the only link to the child is `.`.
                if child.nlink() != 1 {
                    return Err(Ext4Error::new(
                        EIO as _,
                        "new ext4 directory has invalid pre-publication link count",
                    ));
                }
            }
            child.set_mode((child.mode() & !0o7777) | (mode & 0o7777));
            if let Some(now) = initial_time {
                child.set_atime(&now);
                child.set_btime(&now);
                child.set_mtime(&now);
                child.set_ctime(&now);
            }
            parent.add_entry(name, &mut child)?;
            if let Some(now) = initial_time {
                parent.set_mtime(&now);
                parent.set_ctime(&now);
            }
            Ok(())
        })();

        if let Err(primary) = prepare {
            if primary.metadata_may_have_changed() {
                // C has crossed a metadata mutation boundary but could not
                // tell us which subset committed.  Truncating, unlinking, or
                // freeing here could double-release blocks or an inode that a
                // partially published directory entry already references.
                self.metadata_poisoned = true;
                let operation = combine_operation_and_release::<(InodeToken, Arc<AtomicU64>)>(
                    Err(primary),
                    child.finish(),
                );
                return combine_operation_and_release(operation, parent.finish());
            }
            if parent_link_added {
                parent.dec_parent_dir_nlink();
            }
            self.release_inode_handle(token);
            let child_cleanup = self.discard_unpublished_inode(child);
            let cleanup = combine_operation_and_release(child_cleanup, parent.finish());
            let cleanup = self.observe_metadata_result(cleanup);
            if let Err(cleanup) = cleanup {
                log::error!("ext4 create cleanup failed after {primary}: {cleanup}");
            }
            return Err(primary);
        }

        let committed = child.finish().map(|()| (token, namespace_epoch.clone()));
        match combine_operation_and_release(committed, parent.finish()) {
            Ok(handle) => Ok(handle),
            Err(primary) => {
                if primary.metadata_may_have_changed() {
                    self.metadata_poisoned = true;
                    return Err(primary);
                }
                match self.unlink(parent_ino, name) {
                    Ok(()) => {
                        self.release_inode_handle(token);
                        Err(primary)
                    }
                    Err(rollback) => {
                        // The name was committed, but cleanup could not prove
                        // that it removed the same entry.  Further metadata
                        // mutations would build on an ambiguous namespace.
                        self.metadata_poisoned = true;
                        log::error!(
                            "ext4 create rollback failed after reference cleanup error {primary}: \
                             {rollback}"
                        );
                        Err(primary)
                    }
                }
            }
        }
    }

    pub fn create(
        &mut self,
        parent: u32,
        name: &str,
        ty: InodeType,
        mode: u32,
        user: Option<(u32, u32)>,
        rdev: Option<u64>,
        initial_time: Option<Duration>,
    ) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        if ty == InodeType::Symlink {
            return Err(Ext4Error::new(
                ENOTSUP as _,
                "symbolic links require an initialized target",
            ));
        }
        self.create_inner(parent, name, ty, mode, None, user, rdev, initial_time)
    }

    pub fn create_symlink(
        &mut self,
        parent: u32,
        name: &str,
        target: &[u8],
        mode: u32,
        user: Option<(u32, u32)>,
        initial_time: Option<Duration>,
    ) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        self.create_inner(
            parent,
            name,
            InodeType::Symlink,
            mode,
            Some(target),
            user,
            None,
            initial_time,
        )
    }

    pub fn rename(
        &mut self,
        src_dir: u32,
        src_name: &str,
        dst_dir: u32,
        dst_name: &str,
        expected_src: InodeToken,
        expected_dst: Option<InodeToken>,
        timestamp: Option<Duration>,
    ) -> Ext4Result {
        self.ensure_metadata_writable()?;
        self.drain_hot_inodes()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        let (src, src_type) = self.lookup_inode(src_dir, src_name)?;
        let mut src_ref = self.inode_ref(src)?;
        if src_ref.token() != expected_src {
            let err = Ext4Error::new(ENOENT as _, "stale ext4 rename source identity");
            return combine_operation_and_release(Err(err), src_ref.finish());
        }

        let dst = match self.lookup_inode(dst_dir, dst_name) {
            Ok((dst, dst_type)) => {
                let dst_ref = self.inode_ref(dst)?;
                let dst_token = dst_ref.token();
                let validation = if expected_dst != Some(dst_token) {
                    Err(Ext4Error::new(
                        ENOENT as _,
                        "stale ext4 rename destination identity",
                    ))
                } else if dst_token == expected_src {
                    let operation = combine_operation_and_release(Ok(()), dst_ref.finish());
                    return combine_operation_and_release(operation, src_ref.finish());
                } else if src_type == InodeType::Directory && dst_type != InodeType::Directory {
                    Err(Ext4Error::new(ENOTDIR as _, None))
                } else if src_type != InodeType::Directory && dst_type == InodeType::Directory {
                    Err(Ext4Error::new(EISDIR as _, None))
                } else if dst_type == InodeType::Directory
                    && self.clone_ref(&dst_ref)?.has_children()?
                {
                    Err(Ext4Error::new(ENOTEMPTY as _, None))
                } else {
                    Ok(())
                };
                let validation = combine_operation_and_release(validation, dst_ref.finish());
                if let Err(err) = validation {
                    return combine_operation_and_release(Err(err), src_ref.finish());
                }
                Some((dst_token, dst_type))
            }
            Err(err) if err.code == ENOENT as i32 && !err.metadata_may_have_changed() => {
                if expected_dst.is_some() {
                    let stale =
                        Ext4Error::new(ENOENT as _, "missing ext4 rename destination identity");
                    return combine_operation_and_release(Err(stale), src_ref.finish());
                }
                None
            }
            Err(err) => {
                return combine_operation_and_release(Err(err), src_ref.finish());
            }
        };

        let replaced_token = dst.map(|(token, _)| token);
        let destination_type = dst.map(|(_, inode_type)| inode_type);
        if src_type == InodeType::Directory && src_dir != dst_dir {
            // Validate the old `..` relationship before replacement unlink or
            // any transferred-name publication. The post-commit lookup below
            // can then fail only on an underlying metadata I/O fault, which is
            // handled by the existing poisoned-filesystem boundary.
            let parent_validation = (|| {
                let mut result = self.clone_ref(&src_ref)?.lookup("..")?;
                let points_to_source_parent = result.entry()?.ino() == src_dir;
                let validation = if points_to_source_parent {
                    Ok(())
                } else {
                    Err(Ext4Error::new(
                        EIO as _,
                        "ext4 rename source has an invalid parent entry",
                    ))
                };
                combine_operation_and_release(validation, result.finish())
            })();
            let parent_validation = self.observe_metadata_result(parent_validation);
            if let Err(error) = parent_validation {
                return combine_operation_and_release(Err(error), src_ref.finish());
            }
        }
        if rename_requires_destination_parent_link_growth(
            src_type,
            src_dir,
            dst_dir,
            destination_type,
        ) {
            // A cross-parent directory move grows the destination parent's
            // link count unless an existing destination directory first
            // releases one link. Admit that growth before any namespace
            // mutation so EXT4_LINK_MAX cannot wrap or leave a removed victim.
            let destination_parent = self.inode_ref(dst_dir)?;
            let admission = destination_parent.ensure_can_inc_nlink();
            let admission = combine_operation_and_release(admission, destination_parent.finish());
            if let Err(error) = admission {
                return combine_operation_and_release(Err(error), src_ref.finish());
            }
        }

        let had_replacement = dst.is_some();
        let mut mutation_started = false;
        let mut namespace_committed = false;
        if let Some((dst_token, dst_type)) = dst {
            if let Err(err) = self.unlink_checked(
                dst_dir,
                dst_name,
                Some(dst_token),
                Some(dst_type == InodeType::Directory),
                None,
            ) {
                return combine_operation_and_release(Err(err), src_ref.finish());
            }
            mutation_started = true;
        }
        let source_nlink = src_ref.nlink();
        let operation = (|| {
            // Replacement unlink can surface a post-commit cleanup failure as
            // filesystem poison. Stop before publishing the transferred name,
            // while still routing the result through the fail-closed rename
            // completion path below.
            self.ensure_metadata_writable()?;
            if src_dir == dst_dir {
                // The parent and the moved directory's `..` relationship do not
                // change for an in-directory rename. Use one inode reference so a
                // stale duplicate cannot overwrite parent metadata, and do not
                // perturb the parent's link count.
                let mut parent_ref = self.inode_ref(src_dir)?;
                let operation = (|| {
                    if let Err(error) = parent_ref.add_transferred_entry(dst_name, &mut src_ref) {
                        mutation_started |= error.metadata_may_have_changed();
                        return Err(error);
                    }
                    mutation_started = true;
                    if let Err(error) = parent_ref.remove_transferred_entry(src_name) {
                        if !had_replacement && !error.metadata_may_have_changed() {
                            match parent_ref.remove_transferred_entry(dst_name) {
                                Ok(()) => mutation_started = false,
                                Err(rollback_error) => {
                                    return Err(preserve_operation_error(
                                        error,
                                        Err(rollback_error),
                                    ));
                                }
                            }
                        }
                        return Err(error);
                    }
                    // Both externally visible rename decisions are now complete:
                    // the destination names the source inode and the old source
                    // name is gone. Later inode-reference finish failures may
                    // poison metadata, but must not turn this committed rename
                    // into a retryable syscall error.
                    namespace_committed = true;
                    self.apply_committed_rename_timestamps(
                        &mut src_ref,
                        replaced_token,
                        &mut parent_ref,
                        None,
                        timestamp,
                    )?;
                    Ok(())
                })();
                combine_operation_and_release(operation, parent_ref.finish())
            } else {
                let mut src_dir_ref = self.inode_ref(src_dir)?;
                let mut dst_dir_ref = match self.inode_ref(dst_dir) {
                    Ok(dst_dir_ref) => dst_dir_ref,
                    Err(error) => {
                        return combine_operation_and_release(Err(error), src_dir_ref.finish());
                    }
                };

                let operation = (|| {
                    if let Err(error) = dst_dir_ref.add_transferred_entry(dst_name, &mut src_ref) {
                        mutation_started |= error.metadata_may_have_changed();
                        return Err(error);
                    }
                    mutation_started = true;
                    if let Err(error) = src_dir_ref.remove_transferred_entry(src_name) {
                        if !had_replacement && !error.metadata_may_have_changed() {
                            match dst_dir_ref.remove_transferred_entry(dst_name) {
                                Ok(()) => mutation_started = false,
                                Err(rollback_error) => {
                                    return Err(preserve_operation_error(
                                        error,
                                        Err(rollback_error),
                                    ));
                                }
                            }
                        }
                        return Err(error);
                    }
                    namespace_committed = true;

                    if src_ref.is_dir() {
                        // Change `..` and parent directory link counts only for an
                        // actual cross-parent directory move. Destination-parent
                        // growth was admitted before any mutation above.
                        let mut result = self.clone_ref(&src_ref)?.lookup("..")?;
                        result.set_entry_ino(dst_dir)?;
                        result.finish()?;
                        src_dir_ref.dec_parent_dir_nlink();
                        dst_dir_ref.inc_nlink();
                    }
                    self.apply_committed_rename_timestamps(
                        &mut src_ref,
                        replaced_token,
                        &mut src_dir_ref,
                        Some(&mut dst_dir_ref),
                        timestamp,
                    )?;
                    Ok(())
                })();
                let operation = combine_operation_and_release(operation, src_dir_ref.finish());
                combine_operation_and_release(operation, dst_dir_ref.finish())
            }
        })();
        debug_assert_eq!(src_ref.nlink(), source_nlink);
        let operation = combine_operation_and_release(operation, src_ref.finish());
        let operation = complete_namespace_operation(
            &mut self.metadata_poisoned,
            mutation_started,
            namespace_committed,
            "rename metadata finish",
            operation,
        );
        self.observe_metadata_result(operation)
    }

    pub fn link(
        &mut self,
        dir: u32,
        name: &str,
        child: u32,
        ctime: Option<Duration>,
    ) -> Ext4Result {
        self.ensure_metadata_writable()?;
        self.drain_hot_inodes()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        let mut child_ref = self.inode_ref(child)?;
        if child_ref.is_dir() {
            return Err(Ext4Error::new(EISDIR as _, "cannot link to directory"));
        }
        if child_ref.nlink() == 0 {
            return Err(Ext4Error::new(
                ENOENT as _,
                "cannot relink an unlinked inode",
            ));
        }
        let mut dir_ref = self.inode_ref(dir)?;
        if let Err(error) = dir_ref.add_entry(name, &mut child_ref) {
            let operation = combine_operation_and_release::<()>(Err(error), child_ref.finish());
            let operation = combine_operation_and_release(operation, dir_ref.finish());
            return self.observe_metadata_result(operation);
        }
        if let Some(now) = ctime {
            apply_hardlink_timestamps(&mut dir_ref, &mut child_ref, &now);
        }

        // add_entry has committed the name and link-count decision. Source
        // ctime and destination-parent mtime/ctime share the same dirty inode
        // references and lower finish sequence, so no separate fallible
        // metadata operation can escape after publication. A finish failure
        // poisons the filesystem and is logged, while the already committed
        // link remains a successful namespace operation.
        let release = combine_operation_and_release(child_ref.finish(), dir_ref.finish());
        finish_committed_namespace_step(
            &mut self.metadata_poisoned,
            "hard-link metadata finish",
            release,
        );
        Ok(())
    }

    pub fn unlink(&mut self, dir: u32, name: &str) -> Ext4Result {
        self.unlink_checked(dir, name, None, None, None)
    }

    /// Removes one directory entry after validating its stable identity and
    /// type. A VFS caller may supply one timestamp for the committed unlink;
    /// internal rename and rollback callers pass `None` to retain their own
    /// timestamp policy.
    pub fn unlink_checked(
        &mut self,
        dir: u32,
        name: &str,
        expected: Option<InodeToken>,
        is_dir: Option<bool>,
        timestamp: Option<Duration>,
    ) -> Ext4Result {
        self.ensure_metadata_writable()?;
        self.drain_hot_inodes()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        let mut dir_ref = self.inode_ref(dir)?;
        let (child, _) = self.lookup_inode(dir, name)?;
        let mut child_ref = self.inode_ref(child)?;
        let token = child_ref.token();
        let inode_type = child_ref.inode_type();
        let validation = if expected.is_some_and(|expected| expected != token) {
            Err(Ext4Error::new(ENOENT as _, "stale ext4 unlink identity"))
        } else {
            match (inode_type == InodeType::Directory, is_dir) {
                (true, Some(false)) => Err(Ext4Error::new(EISDIR as _, None)),
                (false, Some(true)) => Err(Ext4Error::new(ENOTDIR as _, None)),
                _ => Ok(()),
            }
        };
        if let Err(err) = validation {
            let operation = combine_operation_and_release::<()>(Err(err), child_ref.finish());
            return combine_operation_and_release(operation, dir_ref.finish());
        }
        let link_decrements = if inode_type == InodeType::Directory {
            2
        } else {
            1
        };
        if child_ref.nlink() < link_decrements {
            return Err(Ext4Error::new(
                EIO as _,
                "ext4 inode has invalid link count during unlink",
            ));
        }
        let will_lose_last_link = child_ref.nlink() == link_decrements;

        if self.clone_ref(&child_ref)?.has_children()? {
            return Err(Ext4Error::new(ENOTEMPTY as _, None));
        }
        let prepared_handle_state =
            if will_lose_last_link && !self.inode_handles.contains_key(&token) {
                if self.inode_handles.len() >= MAX_TRACKED_INODE_IDENTITIES {
                    return Err(Ext4Error::new(
                        ENOMEM as _,
                        "ext4 deferred inode deletion limit reached",
                    ));
                }
                self.inode_handles.try_reserve(1).map_err(|_| {
                    Ext4Error::new(ENOMEM as _, "ext4 inode identity allocation failed")
                })?;
                Some(InodeHandleState {
                    handles: 0,
                    pending_delete: None,
                    namespace_epoch: try_namespace_epoch()?,
                })
            } else {
                None
            };
        let mut mutation_started = false;
        if inode_type == InodeType::Directory {
            // According to `ext4_trunc_dir`
            let bs = get_block_size(&self.inner.as_mut().sb);
            if let Err(err) = child_ref.truncate(bs as _) {
                self.metadata_poisoned = true;
                let operation = combine_operation_and_release::<()>(Err(err), child_ref.finish());
                return combine_operation_and_release(operation, dir_ref.finish());
            }
            // Directory contents are already cleared. Even if removing the
            // parent entry later fails cleanly, retrying this operation would
            // build on a partially applied directory mutation.
            mutation_started = true;
        }

        let published_handle_state = prepared_handle_state.is_some();
        if let Some(state) = prepared_handle_state {
            self.inode_handles.insert(token, state);
        }

        if let Err(err) = dir_ref.remove_entry(name, &mut child_ref) {
            let metadata_may_have_changed = err.metadata_may_have_changed();
            if !metadata_may_have_changed && !mutation_started && published_handle_state {
                self.inode_handles.remove(&token);
            }
            let operation = combine_operation_and_release::<()>(Err(err), child_ref.finish());
            let operation = combine_operation_and_release(operation, dir_ref.finish());
            let operation = fail_closed_after_started_mutation(
                &mut self.metadata_poisoned,
                mutation_started,
                operation,
            );
            return self.observe_metadata_result(operation);
        }

        if child_ref.is_dir() {
            dir_ref.dec_parent_dir_nlink();
            child_ref.dec_nlink();
        }
        if let Some(now) = timestamp {
            apply_unlink_timestamps(&mut dir_ref, &mut child_ref, &now);
        }
        if child_ref.nlink() == 0 {
            let Some(state) = self.inode_handles.get_mut(&token) else {
                let err = Ext4Error::new(EIO as _, "missing retained inode state during unlink");
                let operation = combine_operation_and_release::<()>(Err(err), child_ref.finish());
                let operation = combine_operation_and_release(operation, dir_ref.finish());
                finish_committed_namespace_step(
                    &mut self.metadata_poisoned,
                    "unlink retained-inode bookkeeping",
                    operation,
                );
                return Ok(());
            };
            state.pending_delete = Some(PendingDelete::Ready(inode_type));
            unsafe {
                ext4_inode_set_del_time(child_ref.inner.inode, u32::MAX);
            }
            child_ref.mark_dirty();
        }
        let no_handles = self
            .inode_handles
            .get(&token)
            .is_some_and(|state| state.handles == 0 && state.pending_delete.is_some());
        let release = combine_operation_and_release(child_ref.finish(), dir_ref.finish());
        let released = finish_committed_namespace_step(
            &mut self.metadata_poisoned,
            "unlink metadata finish",
            release,
        );
        if released && no_handles {
            let reap = self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET);
            finish_committed_namespace_step(
                &mut self.metadata_poisoned,
                "unlink deferred inode reap",
                reap.map(|_| ()),
            );
        }
        Ok(())
    }

    pub fn stat(&mut self) -> Ext4Result<StatFs> {
        self.ensure_active()?;
        let sb = &mut self.inner.as_mut().sb;
        Ok(StatFs {
            inodes_count: u32::from_le(sb.inodes_count),
            free_inodes_count: u32::from_le(sb.free_inodes_count),
            blocks_count: (u32::from_le(sb.blocks_count_hi) as u64) << 32
                | u32::from_le(sb.blocks_count_lo) as u64,
            free_blocks_count: (u32::from_le(sb.free_blocks_count_hi) as u64) << 32
                | u32::from_le(sb.free_blocks_count_lo) as u64,
            block_size: get_block_size(sb),
        })
    }

    pub fn flush(&mut self) -> Ext4Result<()> {
        self.ensure_active()?;
        let drain = self.drain_hot_inodes();
        let reap = (|| {
            loop {
                let reaped = self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
                if reaped < NAMESPACE_REAP_BUDGET {
                    break;
                }
            }
            Ok(())
        })();
        let mut result = combine_operation_and_release(drain, reap);
        let cache_flush =
            unsafe { ext4_block_cache_flush(self.bdev.inner.as_mut()) }.context("ext4_cache_flush");
        result = combine_operation_and_release(result, cache_flush);
        combine_operation_and_release(
            result,
            self.bdev.dev_mut().flush().context("ext4 device flush"),
        )
    }

    /// Finish all filesystem writeback and close the low-level device.
    ///
    /// Every teardown step is attempted even after an earlier failure. The
    /// first error is retained, and subsequent calls return the same recorded
    /// result without touching the already-finalized cache or device.
    pub fn shutdown(&mut self) -> Ext4Result<()> {
        if let Some(result) = self.shutdown_state.completed_result() {
            return result;
        }

        let mut first_error = None;
        record_shutdown_step(&mut first_error, self.flush());

        if self.metadata_poisoned && first_error.is_none() {
            first_error = Some(Ext4Error::new(
                EIO as _,
                "ext4 metadata state is poisoned during shutdown",
            ));
        }
        if self
            .inode_handles
            .values()
            .any(|state| state.pending_delete.is_some())
        {
            record_shutdown_step(
                &mut first_error,
                Err(Ext4Error::new(
                    EIO as _,
                    "ext4 shutdown still has deferred unlinked inodes",
                )),
            );
        }

        let cleanup = unsafe {
            let bdev = self.bdev.inner.as_mut();
            ext4_bcache_cleanup(bdev.bc).context("ext4_bcache_cleanup")
        };
        record_shutdown_step(&mut first_error, cleanup);
        record_shutdown_step(
            &mut first_error,
            self.bdev
                .dev_mut()
                .flush()
                .context("ext4 cleanup device flush"),
        );

        // The clean marker is the final metadata decision and is written only
        // after every cached buffer has been flushed or accounted as failed.
        let mut clean = first_error.is_none() && !self.metadata_poisoned;
        let superblock_result = unsafe {
            if clean {
                ext4_fs_fini(self.inner.as_mut()).context("ext4_fs_fini")
            } else {
                ext4_fs_fini_error(self.inner.as_mut()).context("ext4_fs_fini_error")
            }
        };
        if let Err(error) = superblock_result {
            record_shutdown_step(&mut first_error, Err(error));
            if clean {
                clean = false;
                let fallback = unsafe {
                    ext4_fs_fini_error(self.inner.as_mut()).context("ext4_fs_fini_error fallback")
                };
                record_shutdown_step(&mut first_error, fallback);
            }
        }

        let final_fence = self
            .bdev
            .dev_mut()
            .flush()
            .context("ext4 final superblock fence");
        if let Err(error) = final_fence {
            record_shutdown_step(&mut first_error, Err(error));
            if clean {
                let mark_error = unsafe {
                    ext4_fs_fini_error(self.inner.as_mut())
                        .context("ext4_fs_fini_error after fence failure")
                };
                record_shutdown_step(&mut first_error, mark_error);
                record_shutdown_step(
                    &mut first_error,
                    self.bdev.dev_mut().flush().context("ext4 ERROR_FS fence"),
                );
            }
        }

        let block_fini =
            unsafe { ext4_block_fini(self.bdev.inner.as_mut()).context("ext4_block_fini") };
        record_shutdown_step(&mut first_error, block_fini);
        let bcache_fini = unsafe {
            let bdev = self.bdev.inner.as_mut();
            ext4_bcache_fini_dynamic(bdev.bc).context("ext4_bcache_fini_dynamic")
        };
        record_shutdown_step(&mut first_error, bcache_fini);

        self.shutdown_state =
            ShutdownState::Complete(first_error.as_ref().map(ShutdownFailure::from_error));
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timestamp_test_inode(fs: &mut ext4_fs, inode: &mut ext4_inode) -> InodeRef<DummyHal> {
        let mut reference = InodeRef::try_uninitialized().unwrap();
        reference.inner.fs = fs;
        reference.inner.inode = inode;
        reference
    }

    #[test]
    fn special_inodes_skip_truncate_during_unlink() {
        assert!(inode_needs_truncate_on_unlink(InodeType::RegularFile));
        assert!(inode_needs_truncate_on_unlink(InodeType::Directory));
        assert!(inode_needs_truncate_on_unlink(InodeType::Symlink));
        assert!(!inode_needs_truncate_on_unlink(InodeType::Socket));
        assert!(!inode_needs_truncate_on_unlink(InodeType::Fifo));
        assert!(!inode_needs_truncate_on_unlink(InodeType::CharacterDevice));
        assert!(!inode_needs_truncate_on_unlink(InodeType::BlockDevice));
    }

    #[test]
    fn hard_link_timestamp_update_covers_source_and_destination_parent() {
        let mut fs = unsafe { mem::zeroed::<ext4_fs>() };
        let mut parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut source_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut parent = timestamp_test_inode(&mut fs, &mut parent_inode);
        let mut source = timestamp_test_inode(&mut fs, &mut source_inode);
        let sentinel = Duration::new(1, 2);
        let now = Duration::new(42, 123_456_789);
        parent.set_mtime(&sentinel);
        parent.set_ctime(&sentinel);
        source.set_ctime(&sentinel);

        apply_hardlink_timestamps(&mut parent, &mut source, &now);

        let mut parent_attr = FileAttr::default();
        parent.get_attr(&mut parent_attr);
        let mut source_attr = FileAttr::default();
        source.get_attr(&mut source_attr);
        assert_eq!(source_attr.ctime, now);
        assert_eq!(parent_attr.mtime, now);
        assert_eq!(parent_attr.ctime, now);
    }

    #[test]
    fn unlink_timestamp_update_covers_victim_and_parent_without_changing_nlink() {
        let mut fs = unsafe { mem::zeroed::<ext4_fs>() };
        let mut parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut victim_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut parent = timestamp_test_inode(&mut fs, &mut parent_inode);
        let mut victim = timestamp_test_inode(&mut fs, &mut victim_inode);
        let sentinel = Duration::new(1, 2);
        let now = Duration::new(42, 123_456_789);
        parent.set_nlink(7);
        victim.set_nlink(2);
        parent.set_mtime(&sentinel);
        parent.set_ctime(&sentinel);
        victim.set_ctime(&sentinel);

        apply_unlink_timestamps(&mut parent, &mut victim, &now);

        let mut parent_attr = FileAttr::default();
        parent.get_attr(&mut parent_attr);
        let mut victim_attr = FileAttr::default();
        victim.get_attr(&mut victim_attr);
        assert_eq!(parent_attr.nlink, 7);
        assert_eq!(victim_attr.nlink, 2);
        assert_eq!(victim_attr.ctime, now);
        assert_eq!(parent_attr.mtime, now);
        assert_eq!(parent_attr.ctime, now);
    }

    #[test]
    fn rename_timestamp_update_covers_every_changed_inode_without_changing_nlinks() {
        let mut fs = unsafe { mem::zeroed::<ext4_fs>() };
        let mut source_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut victim_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut old_parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut new_parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut source = timestamp_test_inode(&mut fs, &mut source_inode);
        let mut victim = timestamp_test_inode(&mut fs, &mut victim_inode);
        let mut old_parent = timestamp_test_inode(&mut fs, &mut old_parent_inode);
        let mut new_parent = timestamp_test_inode(&mut fs, &mut new_parent_inode);
        let sentinel = Duration::new(1, 2);
        let now = Duration::new(42, 123_456_789);
        source.set_nlink(EXT4_LINK_MAX as u16);
        victim.set_nlink(3);
        old_parent.set_nlink(11);
        new_parent.set_nlink(13);
        source.set_ctime(&sentinel);
        victim.set_ctime(&sentinel);
        old_parent.set_mtime(&sentinel);
        old_parent.set_ctime(&sentinel);
        new_parent.set_mtime(&sentinel);
        new_parent.set_ctime(&sentinel);

        apply_rename_timestamps(
            &mut source,
            Some(&mut victim),
            &mut old_parent,
            Some(&mut new_parent),
            &now,
        );

        let mut source_attr = FileAttr::default();
        source.get_attr(&mut source_attr);
        let mut victim_attr = FileAttr::default();
        victim.get_attr(&mut victim_attr);
        let mut old_parent_attr = FileAttr::default();
        old_parent.get_attr(&mut old_parent_attr);
        let mut new_parent_attr = FileAttr::default();
        new_parent.get_attr(&mut new_parent_attr);
        assert_eq!(source_attr.ctime, now);
        assert_eq!(victim_attr.ctime, now);
        assert_eq!(old_parent_attr.mtime, now);
        assert_eq!(old_parent_attr.ctime, now);
        assert_eq!(new_parent_attr.mtime, now);
        assert_eq!(new_parent_attr.ctime, now);
        assert_eq!(source_attr.nlink, EXT4_LINK_MAX as u64);
        assert_eq!(victim_attr.nlink, 3);
        assert_eq!(old_parent_attr.nlink, 11);
        assert_eq!(new_parent_attr.nlink, 13);
    }

    #[test]
    fn same_parent_rename_timestamp_update_does_not_touch_a_second_parent() {
        let mut fs = unsafe { mem::zeroed::<ext4_fs>() };
        let mut source_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut untouched_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut source = timestamp_test_inode(&mut fs, &mut source_inode);
        let mut parent = timestamp_test_inode(&mut fs, &mut parent_inode);
        let mut untouched = timestamp_test_inode(&mut fs, &mut untouched_inode);
        let sentinel = Duration::new(1, 2);
        let now = Duration::new(42, 123_456_789);
        source.set_ctime(&sentinel);
        parent.set_mtime(&sentinel);
        parent.set_ctime(&sentinel);
        untouched.set_mtime(&sentinel);
        untouched.set_ctime(&sentinel);

        apply_rename_timestamps(&mut source, None, &mut parent, None, &now);

        let mut parent_attr = FileAttr::default();
        parent.get_attr(&mut parent_attr);
        let mut untouched_attr = FileAttr::default();
        untouched.get_attr(&mut untouched_attr);
        assert_eq!(parent_attr.mtime, now);
        assert_eq!(parent_attr.ctime, now);
        assert_eq!(untouched_attr.mtime, sentinel);
        assert_eq!(untouched_attr.ctime, sentinel);
    }

    #[test]
    fn rename_parent_link_growth_is_admitted_only_when_the_topology_grows() {
        assert!(!rename_requires_destination_parent_link_growth(
            InodeType::Directory,
            10,
            10,
            None,
        ));
        assert!(rename_requires_destination_parent_link_growth(
            InodeType::Directory,
            10,
            20,
            None,
        ));
        assert!(!rename_requires_destination_parent_link_growth(
            InodeType::Directory,
            10,
            20,
            Some(InodeType::Directory),
        ));
        assert!(!rename_requires_destination_parent_link_growth(
            InodeType::RegularFile,
            10,
            20,
            None,
        ));
    }

    #[test]
    fn rename_parent_link_limit_admission_does_not_wrap_the_inode() {
        let mut fs = unsafe { mem::zeroed::<ext4_fs>() };
        let mut parent_inode = unsafe { mem::zeroed::<ext4_inode>() };
        let mut parent = timestamp_test_inode(&mut fs, &mut parent_inode);
        let sentinel = Duration::new(1, 2);
        parent.set_nlink(EXT4_LINK_MAX as u16);
        parent.set_mtime(&sentinel);
        parent.set_ctime(&sentinel);

        let error = parent.ensure_can_inc_nlink().unwrap_err();

        let mut parent_attr = FileAttr::default();
        parent.get_attr(&mut parent_attr);
        assert_eq!(error.code, EMLINK as i32);
        assert_eq!(parent.nlink(), EXT4_LINK_MAX as u16);
        assert_eq!(parent_attr.mtime, sentinel);
        assert_eq!(parent_attr.ctime, sentinel);
    }

    #[test]
    fn stale_token_release_does_not_touch_replacement() {
        let stale = InodeToken::new(42, 7);
        let replacement = InodeToken::new(42, 8);
        let mut handles = HashMap::from([(
            replacement,
            InodeHandleState {
                handles: 1,
                pending_delete: None,
                namespace_epoch: Arc::new(AtomicU64::new(0)),
            },
        )]);

        assert_eq!(
            release_inode_handle_state(&mut handles, stale),
            HandleRelease::Untracked
        );
        assert_eq!(handles.get(&replacement).unwrap().handles, 1);
    }

    #[test]
    fn pending_delete_survives_last_handle_until_reap() {
        let token = InodeToken::new(17, 3);
        let mut handles = HashMap::from([(
            token,
            InodeHandleState {
                handles: 1,
                pending_delete: Some(PendingDelete::Ready(InodeType::RegularFile)),
                namespace_epoch: Arc::new(AtomicU64::new(0)),
            },
        )]);

        assert_eq!(
            release_inode_handle_state(&mut handles, token),
            HandleRelease::Retained
        );
        let state = handles.get(&token).unwrap();
        assert_eq!(state.handles, 0);
        assert_eq!(
            state.pending_delete,
            Some(PendingDelete::Ready(InodeType::RegularFile))
        );
    }

    #[test]
    fn namespace_failure_order_is_fail_closed_after_mutation_starts() {
        let clean_error = Ext4Error::new(EIO as _, "pre-publication failure");
        let mut poisoned = false;
        assert!(
            fail_closed_after_started_mutation(&mut poisoned, false, Err::<(), _>(clean_error))
                .is_err()
        );
        assert!(!poisoned);

        let partial_error = Ext4Error::new(EIO as _, "partial namespace mutation");
        assert!(
            fail_closed_after_started_mutation(&mut poisoned, true, Err::<(), _>(partial_error))
                .is_err()
        );
        assert!(poisoned);
    }

    #[test]
    fn committed_namespace_cleanup_failure_poison_is_not_retryable() {
        let mut poisoned = false;
        assert!(!finish_committed_namespace_step(
            &mut poisoned,
            "test committed mutation",
            Err(Ext4Error::new(EIO as _, "post-commit writeback failure")),
        ));
        assert!(poisoned);
    }

    #[test]
    fn rename_cleanup_failure_after_namespace_commit_is_not_retryable() {
        let mut poisoned = false;
        let result = complete_namespace_operation(
            &mut poisoned,
            true,
            true,
            "rename metadata finish",
            Err(Ext4Error::new(
                EIO as _,
                "rename post-commit finish failure",
            )),
        );

        assert!(result.is_ok());
        assert!(poisoned);
    }
}

impl<Hal: SystemHal, Dev: BlockDevice> Drop for Ext4Filesystem<Hal, Dev> {
    fn drop(&mut self) {
        if matches!(self.shutdown_state, ShutdownState::Active)
            && let Err(error) = self.shutdown()
        {
            log::error!("ext4 shutdown failed during drop: {error}");
        }
    }
}

pub(crate) struct WritebackGuard {
    bdev: *mut ext4_blockdev,
}
impl WritebackGuard {
    pub fn new(bdev: *mut ext4_blockdev) -> Self {
        unsafe { ext4_block_cache_write_back(bdev, 1) };
        Self { bdev }
    }
}
impl Drop for WritebackGuard {
    fn drop(&mut self) {
        unsafe { ext4_block_cache_write_back(self.bdev, 0) };
    }
}
