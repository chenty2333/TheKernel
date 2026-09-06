use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use hashbrown::HashMap;

use crate::{
    DirLookupResult, DirReader, Ext4Error, Ext4Result, FileAttr, InodeRef, InodeToken, InodeType,
    blockdev::{
        AsyncReadSubmission, AsyncWriteSubmission, BlockDevice, EXT4_DEV_BSIZE, Ext4BlockDevice,
        PhysicalIoBatchRequest, PhysicalIoBatchSubmitOutcome, PhysicalIoNotSubmittedReason,
        PhysicalIoSegment,
    },
    error::Context,
    ffi::*,
    hot::{
        ENABLE_HOT_INODE_CACHE, HotInodeCache, async_mapped_read_enabled, record_async_mapped_read,
        record_async_mapped_read_cookie_reject, record_async_mapped_read_fallback,
        record_hot_inode_hit, record_hot_inode_miss, record_inode_ref_get,
        record_mapped_overwrite_vectored_hit, record_mapped_read_vectored,
    },
    iomap::{MappedRun, MappedRunKind},
    util::get_block_size,
};

#[cfg(test)]
use crate::blockdev::PhysicalIoBatchSubmission;

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
    filesystem_id: u64,
    inner: Box<ext4_fs>,
    bdev: Ext4BlockDevice<Dev>,
    hot_inodes: HotInodeCache<Hal>,
    inode_handles: HashMap<InodeToken, InodeHandleState>,
    metadata_poisoned: bool,
    shutdown_state: ShutdownState,
    _phantom: PhantomData<Hal>,
}

static NEXT_FILESYSTEM_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_WHITEOUT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

fn whiteout_staging_name() -> Vec<u8> {
    let value = NEXT_WHITEOUT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut name = Vec::from(&b".wh.renameat2."[..]);
    for shift in (0..16).rev() {
        let digit = ((value >> (shift * 4)) & 0xf) as u8;
        name.push(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        });
    }
    name
}

/// An explicit lwext4 JBD scope.  It owns only the C transaction pointer, so
/// callers may continue using `&mut Ext4Filesystem` while the scope is live.
/// Dropping an uncommitted scope aborts every dirty block collected so far.
struct Ext4JournalScope {
    fs: *mut ext4_fs,
    owns: bool,
    finished: bool,
}

impl Ext4JournalScope {
    fn begin(fs: *mut ext4_fs) -> Ext4Result<Self> {
        let owns = unsafe { (*fs).curr_trans.is_null() };
        unsafe { ext4_fs_trans_start(fs) }.context("ext4_fs_trans_start")?;
        Ok(Self {
            fs,
            owns,
            finished: false,
        })
    }

    fn commit(mut self) -> Ext4Result<()> {
        let result = if self.owns {
            unsafe { ext4_fs_trans_stop(self.fs) }.context("ext4_fs_trans_stop")
        } else {
            Ok(())
        };
        self.finished = true;
        result
    }
}

impl Drop for Ext4JournalScope {
    fn drop(&mut self) {
        if !self.finished && self.owns {
            unsafe { ext4_fs_trans_abort(self.fs) };
        }
    }
}

#[derive(Clone, Copy)]
struct ShutdownFailure {
    code: i32,
    context: Option<&'static str>,
    metadata_may_have_changed: bool,
    physical_quarantined: bool,
}

impl ShutdownFailure {
    fn from_error(error: &Ext4Error) -> Self {
        Self {
            code: error.code,
            context: error.context,
            metadata_may_have_changed: error.metadata_may_have_changed(),
            physical_quarantined: error.physical_quarantined(),
        }
    }

    fn into_error(self) -> Ext4Error {
        Ext4Error::new(self.code, self.context)
            .with_metadata_may_have_changed(self.metadata_may_have_changed)
            .with_physical_quarantined(self.physical_quarantined)
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
/// Maximum number of mapped extents in one physical direct-I/O effect.
///
/// This is deliberately the same bound as the lower physical route's child
/// capacity. Keeping it here makes filesystem admission finite before any
/// queue state or DMA mapping is touched.
pub const MAX_PHYSICAL_IO_EXTENTS: usize = 16;
const MAX_PHYSICAL_IO_INPUT_SEGMENTS: usize = 16;
/// Maximum number of owned physical SG slices in one effect.  A source SG
/// segment may be split at an extent boundary, so this is a bound on the
/// normalized slices retained by the plan rather than on the caller's input
/// count alone.  Intersecting at most E extents with at most S disjoint source
/// segments creates at most E + S - 1 non-empty slices.
pub const MAX_PHYSICAL_IO_SEGMENTS: usize =
    MAX_PHYSICAL_IO_EXTENTS + MAX_PHYSICAL_IO_INPUT_SEGMENTS - 1;
/// Maximum direct-I/O payload admitted by the ext4 physical fast path.
pub const MAX_PHYSICAL_IO_BYTES: usize = 256 * 1024;

/// The direction of one prepared physical filesystem operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoOperation {
    Read,
    Write,
}

impl PhysicalIoOperation {
    pub const fn is_write(self) -> bool {
        matches!(self, Self::Write)
    }
}

fn physical_io_read_lengths(
    file_size: u64,
    offset: u64,
    len: usize,
    block_size: usize,
    overwrite_only: bool,
) -> Option<(usize, usize)> {
    if len == 0 || block_size == 0 || offset % block_size as u64 != 0 || len % block_size != 0 {
        return None;
    }
    let request_end = offset.checked_add(len as u64)?;
    if offset >= file_size || overwrite_only && request_end > file_size {
        return None;
    }
    let logical_bytes = len.min(usize::try_from(file_size - offset).unwrap_or(usize::MAX));
    if logical_bytes == 0 {
        return None;
    }
    let io_bytes = logical_bytes.checked_add(block_size - 1)? / block_size * block_size;
    // A physical direct read must never write past the caller-visible short
    // count. A future mixed tail plan may relax this with a private bounce
    // block, but the current zero-copy path deliberately falls back.
    (io_bytes <= len && logical_bytes == io_bytes).then_some((logical_bytes, io_bytes))
}

/// One written ext4 extent included in a bounded physical I/O plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoExtent {
    file_offset: u64,
    pblock: u64,
    physical_block_id: u64,
    bytes: usize,
    blocks: u32,
    /// Index of the first owned SG slice for this extent.
    segment_start: u8,
    /// Number of owned SG slices for this extent.
    segment_count: u8,
}

impl PhysicalIoExtent {
    pub fn file_offset(self) -> u64 {
        self.file_offset
    }

    pub fn physical_block_id(self) -> u64 {
        self.physical_block_id
    }

    pub fn bytes(self) -> usize {
        self.bytes
    }

    pub fn blocks(self) -> u32 {
        self.blocks
    }

    pub fn segment_start(self) -> usize {
        usize::from(self.segment_start)
    }

    pub fn segment_count(self) -> usize {
        usize::from(self.segment_count)
    }
}

/// A validated, bounded physical I/O plan. The plan is copyable so the
/// caller can release the ext4 filesystem lock before entering synchronous
/// device waits, then revalidate the complete mapping before publishing a
/// write-cache invalidation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoPlan {
    ino: u32,
    /// Stable identity of the ext4 filesystem instance which produced this
    /// plan.  The owner keeps the filesystem alive; the identity additionally
    /// prevents accidentally finalizing a plan against a replacement mount.
    filesystem_id: u64,
    file_offset: u64,
    /// Bytes visible to the caller. The current physical path requires this
    /// to equal `io_bytes`; partial-block EOF reads use the bounce path.
    logical_bytes: usize,
    io_bytes: usize,
    operation: PhysicalIoOperation,
    extent_count: usize,
    extents: [PhysicalIoExtent; MAX_PHYSICAL_IO_EXTENTS],
    segment_count: usize,
    segments: [PhysicalIoSegment; MAX_PHYSICAL_IO_SEGMENTS],
    mapping_seq: u64,
}

impl PhysicalIoPlan {
    pub fn extent_count(self) -> usize {
        self.extent_count
    }

    pub fn bytes(self) -> usize {
        self.logical_bytes
    }

    pub fn io_bytes(self) -> usize {
        self.io_bytes
    }

    pub fn operation(self) -> PhysicalIoOperation {
        self.operation
    }

    pub fn inode(self) -> u32 {
        self.ino
    }

    pub fn filesystem_id(self) -> u64 {
        self.filesystem_id
    }

    pub fn mapping_seq(self) -> u64 {
        self.mapping_seq
    }

    pub fn extent(self, index: usize) -> Option<PhysicalIoExtent> {
        (index < self.extent_count).then_some(self.extents[index])
    }

    pub fn segment_count(self) -> usize {
        self.segment_count
    }

    pub fn segment(self, index: usize) -> Option<PhysicalIoSegment> {
        (index < self.segment_count).then_some(self.segments[index])
    }

    pub fn segments(&self) -> &[PhysicalIoSegment] {
        &self.segments[..self.segment_count]
    }
}

/// Exact completion information supplied by the physical effect owner.  A
/// driver completion cookie is intentionally independent from the raw handle;
/// both must be checked before a plan can be finalized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoCompletion {
    pub handle: u64,
    pub cookie: u64,
    pub bytes: usize,
    pub success: bool,
}

/// Why an effect must remain owned instead of being settled.  These are
/// protocol/retirement conditions, not ordinary I/O errors: the caller must
/// retain the pin, range lease, and cache owner and may submit later exact
/// completions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoPendingReason {
    NotPublished,
    /// The device reported an accepted request for which the effect owner
    /// cannot retain an exact handle (or reported an impossible count).  The
    /// known prefix may still be drained, but the effect is never drop-safe.
    MalformedPublication,
    MissingCompletion {
        observed: usize,
        expected: usize,
    },
    UnknownHandle,
    DuplicateCompletion,
    CookieMismatch,
    BytesMismatch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoCompletionOutcome {
    Accepted,
    Retain(PhysicalIoPendingReason),
}

/// A physical effect is settled only once every handle that was actually
/// published has an exact retirement observation.  `success` is the logical
/// result after all statuses are known; false covers device errors and a
/// terminal partial publication, but is still safe to release physically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoSettlement {
    Settled { plan: PhysicalIoPlan, success: bool },
    Retain(PhysicalIoPendingReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalIoPublication {
    handles: [u64; MAX_PHYSICAL_IO_EXTENTS],
    cookies: [u64; MAX_PHYSICAL_IO_EXTENTS],
    cookie_known: [bool; MAX_PHYSICAL_IO_EXTENTS],
    count: usize,
    bytes: usize,
    terminal: bool,
}

impl PhysicalIoPublication {
    pub fn count(self) -> usize {
        self.count
    }

    pub fn bytes(self) -> usize {
        self.bytes
    }

    pub fn handle(self, index: usize) -> Option<u64> {
        (index < self.count).then_some(self.handles[index])
    }

    pub fn cookie(self, index: usize) -> Option<u64> {
        (index < self.count && self.cookie_known[index]).then_some(self.cookies[index])
    }

    pub fn terminal(self) -> bool {
        self.terminal
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoPublishOutcome {
    /// The driver performed no publication; synchronous fallback remains
    /// valid for this prepared effect.
    NotSubmitted(PhysicalIoNotSubmittedReason),
    /// Every extent was accepted and the owner may await exact completions.
    Published(PhysicalIoPublication),
    /// A prefix or malformed byte report was accepted.  This is terminal and
    /// can never be converted into a fallback operation.
    Terminal(PhysicalIoPublication),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoEffectState {
    Prepared,
    Published,
    Terminal,
    Quarantined,
    Completed,
    SettledFailure,
    Finalized,
}

/// Owned, bounded state for one physical ext4 effect.  It contains no caller
/// borrow and no virtual address; the upper layer retains the pin owner and
/// range lease alongside this value until all exact completions have settled.
#[derive(Debug)]
pub struct PhysicalIoEffect {
    plan: PhysicalIoPlan,
    state: PhysicalIoEffectState,
    publication: PhysicalIoPublication,
    /// A malformed report may have accepted requests for which no exact
    /// handle was returned.  Keep this bit independent of the retained
    /// prefix so a complete prefix can never incorrectly become drop-safe.
    publication_quarantined: bool,
    /// A malformed/duplicate completion is reset-required, not an ordinary
    /// logical I/O failure. Keep the reason so settlement can never release
    /// the upper physical owner after returning EIO.
    quarantine_reason: Option<PhysicalIoPendingReason>,
    completed: [bool; MAX_PHYSICAL_IO_EXTENTS],
    completion_count: usize,
    logical_success: bool,
}

impl PhysicalIoEffect {
    pub fn new(plan: PhysicalIoPlan) -> Self {
        Self {
            plan,
            state: PhysicalIoEffectState::Prepared,
            publication: PhysicalIoPublication {
                handles: [0; MAX_PHYSICAL_IO_EXTENTS],
                cookies: [0; MAX_PHYSICAL_IO_EXTENTS],
                cookie_known: [false; MAX_PHYSICAL_IO_EXTENTS],
                count: 0,
                bytes: 0,
                terminal: false,
            },
            publication_quarantined: false,
            quarantine_reason: None,
            completed: [false; MAX_PHYSICAL_IO_EXTENTS],
            completion_count: 0,
            logical_success: true,
        }
    }

    pub fn plan(&self) -> PhysicalIoPlan {
        self.plan
    }

    pub fn state(&self) -> PhysicalIoEffectState {
        self.state
    }

    pub fn publication(&self) -> Option<PhysicalIoPublication> {
        (self.publication.count != 0).then_some(self.publication)
    }

    /// Publishes every extent through one caller-supplied atomic driver hook.
    /// The hook receives only owned fixed request copies and must return
    /// `None` when it can prove that no descriptor was visible.
    pub unsafe fn publish_with(
        &mut self,
        submit: impl FnOnce(&[PhysicalIoBatchRequest]) -> Ext4Result<PhysicalIoBatchSubmitOutcome>,
    ) -> Ext4Result<PhysicalIoPublishOutcome> {
        if self.state != PhysicalIoEffectState::Prepared {
            return Err(Ext4Error::new(
                EINVAL as _,
                "physical effect already published",
            ));
        }
        let mut requests = [PhysicalIoBatchRequest::empty(); MAX_PHYSICAL_IO_EXTENTS];
        for index in 0..self.plan.extent_count {
            requests[index] = PhysicalIoBatchRequest::from_plan(self.plan, index)
                .ok_or_else(|| Ext4Error::new(EINVAL as _, "invalid physical effect extent"))?;
        }
        let submission = match submit(&requests[..self.plan.extent_count])? {
            PhysicalIoBatchSubmitOutcome::NotSubmitted(reason) => {
                // The effect remains Prepared.  Since the lower contract
                // proves that no descriptor was visible, the same prepared
                // effect can safely be retried after transient backpressure.
                return Ok(PhysicalIoPublishOutcome::NotSubmitted(reason));
            }
            PhysicalIoBatchSubmitOutcome::Submitted(submission) => submission,
        };
        let malformed_count = submission.submitted > MAX_PHYSICAL_IO_EXTENTS;
        let malformed_handles = submission.handles.len() != submission.submitted;
        let malformed_cookies = submission.submitted != 0
            && (submission.cookies.len() != submission.submitted
                || submission.cookies.iter().any(|cookie| *cookie == 0));
        let retained_count = submission
            .submitted
            .min(submission.handles.len())
            .min(MAX_PHYSICAL_IO_EXTENTS);
        let retained_handles = submission.handles.iter().copied().take(retained_count);
        self.publication = PhysicalIoPublication {
            handles: {
                let mut handles = [0; MAX_PHYSICAL_IO_EXTENTS];
                for (index, handle) in retained_handles.enumerate() {
                    handles[index] = handle;
                }
                handles
            },
            cookies: {
                let mut cookies = [0; MAX_PHYSICAL_IO_EXTENTS];
                for (index, cookie) in submission.cookies.iter().copied().enumerate() {
                    if index == retained_count {
                        break;
                    }
                    cookies[index] = cookie;
                }
                cookies
            },
            cookie_known: {
                let mut known = [false; MAX_PHYSICAL_IO_EXTENTS];
                if !submission.cookies.is_empty() && !malformed_cookies {
                    for known in known.iter_mut().take(retained_count) {
                        *known = true;
                    }
                }
                known
            },
            count: retained_count,
            bytes: submission.bytes,
            terminal: malformed_count
                || malformed_handles
                || malformed_cookies
                || submission.terminal
                || submission.submitted != self.plan.extent_count
                || submission.bytes != self.plan.io_bytes,
        };
        self.publication_quarantined = malformed_count
            || malformed_handles
            || malformed_cookies
            || (submission.submitted == 0 && (submission.terminal || submission.bytes != 0));
        self.state = if self.publication.terminal {
            PhysicalIoEffectState::Terminal
        } else {
            PhysicalIoEffectState::Published
        };
        let publication = self.publication;
        Ok(if publication.terminal {
            PhysicalIoPublishOutcome::Terminal(publication)
        } else {
            PhysicalIoPublishOutcome::Published(publication)
        })
    }

    fn quarantine(&mut self, reason: PhysicalIoPendingReason) {
        self.state = PhysicalIoEffectState::Quarantined;
        self.quarantine_reason.get_or_insert(reason);
    }

    /// Records one completion observation.  Device failure is an accepted
    /// retirement and only changes the eventual logical result.  Unknown,
    /// duplicate, cookie-mismatched, and short observations are retained as
    /// quarantine reasons; they never make the effect drop-safe.
    pub fn record_completion(
        &mut self,
        completion: PhysicalIoCompletion,
    ) -> PhysicalIoCompletionOutcome {
        if !matches!(
            self.state,
            PhysicalIoEffectState::Published
                | PhysicalIoEffectState::Terminal
                | PhysicalIoEffectState::Quarantined
        ) {
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::NotPublished);
        }
        if completion.handle == 0 {
            self.quarantine(PhysicalIoPendingReason::UnknownHandle);
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::UnknownHandle);
        }
        if completion.cookie == 0 {
            self.quarantine(PhysicalIoPendingReason::CookieMismatch);
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::CookieMismatch);
        }
        let Some(index) = (0..self.publication.count)
            .find(|index| self.publication.handles[*index] == completion.handle)
        else {
            self.quarantine(PhysicalIoPendingReason::UnknownHandle);
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::UnknownHandle);
        };
        if self.completed[index] {
            let reason = if self.publication.cookie_known[index]
                && self.publication.cookies[index] != completion.cookie
            {
                PhysicalIoPendingReason::CookieMismatch
            } else {
                PhysicalIoPendingReason::DuplicateCompletion
            };
            self.quarantine(reason);
            return PhysicalIoCompletionOutcome::Retain(reason);
        }
        if self.publication.cookie_known[index]
            && self.publication.cookies[index] != completion.cookie
        {
            self.quarantine(PhysicalIoPendingReason::CookieMismatch);
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::CookieMismatch);
        }
        if !self.publication.cookie_known[index] {
            self.publication.cookies[index] = completion.cookie;
            self.publication.cookie_known[index] = true;
        }
        let expected = self.plan.extent(index).map(|extent| extent.bytes());
        let bytes_match = if !completion.success {
            // A device-error status is already a terminal logical failure;
            // used length is commonly zero and does not describe a partial
            // caller-visible read.  Handle/cookie identity still proves that
            // this exact request retired.
            true
        } else {
            match expected {
                Some(expected) => match self.plan.operation {
                    // Several virtio backends report a successful write with
                    // a used length of zero.  A non-zero write length is still
                    // checked when the backend provides one; reads require
                    // the exact extent length because those bytes fill the
                    // caller SG.
                    PhysicalIoOperation::Write => {
                        completion.bytes == 0 || completion.bytes == expected
                    }
                    PhysicalIoOperation::Read => completion.bytes == expected,
                },
                None => false,
            }
        };
        if !bytes_match {
            self.quarantine(PhysicalIoPendingReason::BytesMismatch);
            return PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::BytesMismatch);
        }
        self.completed[index] = true;
        self.completion_count = self.completion_count.saturating_add(1);
        if !completion.success {
            self.logical_success = false;
        }
        PhysicalIoCompletionOutcome::Accepted
    }

    /// Settles the physical owner only after every actually published handle
    /// has an exact completion observation.  A terminal partial publication
    /// can therefore settle to a logical EIO once its accepted prefix retires.
    pub fn settle(&mut self) -> PhysicalIoSettlement {
        if self.state == PhysicalIoEffectState::Prepared {
            return PhysicalIoSettlement::Retain(PhysicalIoPendingReason::NotPublished);
        }
        if self.state == PhysicalIoEffectState::Finalized {
            return PhysicalIoSettlement::Retain(PhysicalIoPendingReason::NotPublished);
        }
        if self.publication_quarantined {
            return PhysicalIoSettlement::Retain(PhysicalIoPendingReason::MalformedPublication);
        }
        if let Some(reason) = self.quarantine_reason {
            return PhysicalIoSettlement::Retain(reason);
        }
        if self.completion_count != self.publication.count {
            return PhysicalIoSettlement::Retain(PhysicalIoPendingReason::MissingCompletion {
                observed: self.completion_count,
                expected: self.publication.count,
            });
        }
        let success = self.logical_success
            && !self.publication.terminal
            && self.state != PhysicalIoEffectState::Quarantined;
        self.state = if success {
            PhysicalIoEffectState::Completed
        } else {
            PhysicalIoEffectState::SettledFailure
        };
        PhysicalIoSettlement::Settled {
            plan: self.plan,
            success,
        }
    }

    pub fn mark_finalized(&mut self) -> Ext4Result<PhysicalIoPlan> {
        if !matches!(
            self.state,
            PhysicalIoEffectState::Completed | PhysicalIoEffectState::SettledFailure
        ) {
            return Err(Ext4Error::new(EIO as _, "physical effect is not complete"));
        }
        self.state = PhysicalIoEffectState::Finalized;
        Ok(self.plan)
    }
}

/// Copies a caller SG list into a fixed, extent-sliced representation owned by
/// the plan.  The input is only borrowed during preparation; no pointer or
/// borrow is retained after this function returns.
fn copy_physical_segments_into_plan(
    plan: &mut PhysicalIoPlan,
    input: &[PhysicalIoSegment],
) -> bool {
    if input.is_empty()
        || input.len() > MAX_PHYSICAL_IO_INPUT_SEGMENTS
        || plan.io_bytes == 0
        || plan.io_bytes > MAX_PHYSICAL_IO_BYTES
    {
        return false;
    }

    let mut ranges = [(0usize, 0usize); MAX_PHYSICAL_IO_INPUT_SEGMENTS];
    let mut input_total = 0usize;
    for (index, segment) in input.iter().copied().enumerate() {
        if segment.len == 0
            || segment.paddr % EXT4_DEV_BSIZE != 0
            || segment.len % EXT4_DEV_BSIZE != 0
        {
            return false;
        }
        let Some(end) = segment.paddr.checked_add(segment.len) else {
            return false;
        };
        input_total = match input_total.checked_add(segment.len) {
            Some(total) => total,
            None => return false,
        };
        ranges[index] = (segment.paddr, end);
    }
    if input_total != plan.io_bytes {
        return false;
    }
    ranges[..input.len()].sort_unstable_by_key(|range| range.0);
    if ranges[..input.len()]
        .windows(2)
        .any(|pair| pair[0].1 > pair[1].0)
    {
        return false;
    }

    let mut input_index = 0usize;
    let mut input_offset = 0usize;
    let mut output_count = 0usize;
    for extent_index in 0..plan.extent_count {
        let extent = plan.extents[extent_index];
        let start = output_count;
        let mut remaining = extent.bytes;
        while remaining != 0 {
            let Some(segment) = input.get(input_index).copied() else {
                return false;
            };
            if input_offset == segment.len {
                input_index = match input_index.checked_add(1) {
                    Some(index) => index,
                    None => return false,
                };
                input_offset = 0;
                continue;
            }
            let Some(paddr) = segment.paddr.checked_add(input_offset) else {
                return false;
            };
            let take = remaining.min(segment.len - input_offset);
            if take == 0 || take % EXT4_DEV_BSIZE != 0 || paddr % EXT4_DEV_BSIZE != 0 {
                return false;
            }
            if output_count != start {
                let previous = plan.segments[output_count - 1];
                if previous.paddr.checked_add(previous.len) == Some(paddr) {
                    plan.segments[output_count - 1].len = match previous.len.checked_add(take) {
                        Some(len) => len,
                        None => return false,
                    };
                } else {
                    if output_count >= MAX_PHYSICAL_IO_SEGMENTS {
                        return false;
                    }
                    plan.segments[output_count] = PhysicalIoSegment { paddr, len: take };
                    output_count += 1;
                }
            } else {
                if output_count >= MAX_PHYSICAL_IO_SEGMENTS {
                    return false;
                }
                plan.segments[output_count] = PhysicalIoSegment { paddr, len: take };
                output_count += 1;
            }
            input_offset = match input_offset.checked_add(take) {
                Some(offset) => offset,
                None => return false,
            };
            remaining -= take;
        }
        let count = output_count - start;
        if count == 0 || start > usize::from(u8::MAX) || count > usize::from(u8::MAX) {
            return false;
        }
        plan.extents[extent_index].segment_start = start as u8;
        plan.extents[extent_index].segment_count = count as u8;
    }
    if input_index < input.len()
        && (input_offset != input[input_index].len || input_index + 1 != input.len())
    {
        return false;
    }
    if output_count == 0 || output_count > MAX_PHYSICAL_IO_SEGMENTS {
        return false;
    }
    plan.segment_count = output_count;
    true
}

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
    fn journal_scope(&mut self) -> Ext4Result<Ext4JournalScope> {
        if self.inner.jbd_journal.is_null() {
            return Err(Ext4Error::new(
                ENOTSUP as _,
                "atomic renameat2 requires an ext4 journal",
            ));
        }
        Ext4JournalScope::begin(self.inner.as_mut() as *mut ext4_fs)
    }

    pub fn supports_atomic_renameat2(&self) -> bool {
        !self.inner.jbd_journal.is_null()
    }

    /// Returns the exclusive byte bound addressable by this ext4 mapper.
    pub fn max_extent_bytes(&self) -> u64 {
        u64::from(get_block_size(&self.inner.sb)).saturating_mul(u64::from(u32::MAX) + 1)
    }

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
                filesystem_id: NEXT_FILESYSTEM_ID.fetch_add(1, Ordering::Relaxed),
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

    /// Stable identity for this mounted filesystem instance.  It is captured
    /// in every physical plan so a plan cannot be finalized against a
    /// replacement mount which happens to reuse the same inode number.
    pub fn filesystem_id(&self) -> u64 {
        self.filesystem_id
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

    pub fn lookup_inode(&mut self, parent: u32, name: &[u8]) -> Ext4Result<(u32, InodeType)> {
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
        self.with_inode_ref_mut(ino, |inode| {
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
        self.with_inode_ref(ino, |inode| {
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
        // A submit-only call must never accept one extent and then wait while
        // the caller still owns the filesystem lock. Fragmented mappings use
        // the synchronous path until the API can return an owned partial
        // submission transaction for reaping after unlock.
        if runs.len() != 1 {
            return Ok(None);
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

        let run = &runs[0];
        if run.bytes != bytes {
            return Ok(None);
        }
        let block_id = self.bdev.direct_physical_block_id(run.pblock);
        let Some(submission) = self
            .bdev
            .dev_mut()
            .try_read_blocks_vectored_async_submit(block_id, bufs)?
        else {
            return Ok(None);
        };

        debug_assert!(submission.bytes <= bytes);
        record_async_mapped_read(1, submission.bytes, submission.submit_batches);
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

    /// Returns whether the one-based inode number is set in the on-disk inode
    /// bitmap.
    pub fn inode_is_allocated(&mut self, ino: u32) -> Ext4Result<bool> {
        let mut allocated = false;
        unsafe {
            ext4_fs_inode_is_allocated(self.inner.as_mut(), ino, &mut allocated)
                .context("ext4_fs_inode_is_allocated")?;
        }
        Ok(allocated)
    }

    /// Visits every allocated inode-table entry.
    ///
    /// The allocation decision comes from the on-disk inode bitmap, rather
    /// than from an inode-table heuristic. Visiting one table entry at a time
    /// bounds memory and releases its cache reference before advancing.
    pub fn enumerate_allocated_inodes(
        &mut self,
        visitor: &mut dyn FnMut(FileAttr) -> Ext4Result<()>,
    ) -> Ext4Result<()> {
        let count = self.stat()?.inodes_count;
        for ino in 1..=count {
            if !self.inode_is_allocated(ino)? {
                continue;
            }
            let mut attr = FileAttr::default();
            self.get_attr(ino, &mut attr)?;
            visitor(attr)?;
        }
        Ok(())
    }

    /// Builds a single fully-written contiguous extent plan while the caller
    /// owns the ext4 filesystem lock. No device operation is performed here;
    /// the copyable plan lets the caller release that lock before a
    /// synchronous driver wait.
    pub fn plan_physical_io(
        &mut self,
        ino: u32,
        offset: u64,
        len: usize,
        overwrite_only: bool,
    ) -> Ext4Result<Option<PhysicalIoPlan>> {
        if len == 0
            || len > MAX_PHYSICAL_IO_BYTES
            || offset % EXT4_DEV_BSIZE as u64 != 0
            || len % EXT4_DEV_BSIZE != 0
        {
            return Ok(None);
        }
        self.with_inode_ref_mut(ino, |inode| {
            let block_size = get_block_size(inode.superblock());
            if block_size != 4096
                || offset % block_size as u64 != 0
                || len % block_size as usize != 0
                || inode.inode_type() != InodeType::RegularFile
            {
                return Ok(None);
            }
            let file_size = inode.size();
            let Some((logical_bytes, io_bytes)) = physical_io_read_lengths(
                file_size,
                offset,
                len,
                block_size as usize,
                overwrite_only,
            ) else {
                return Ok(None);
            };
            let Some(runs) =
                inode.map_iomap_runs_without_cache(offset, io_bytes, overwrite_only)?
            else {
                return Ok(None);
            };
            if runs.is_empty()
                || runs.len() > MAX_PHYSICAL_IO_EXTENTS
                || runs.iter().any(|run| {
                    run.kind != MappedRunKind::Written || run.pblock == 0 || run.bytes == 0
                })
            {
                return Ok(None);
            }
            let mut extents = [PhysicalIoExtent {
                file_offset: 0,
                pblock: 0,
                physical_block_id: 0,
                bytes: 0,
                blocks: 0,
                segment_start: 0,
                segment_count: 0,
            }; MAX_PHYSICAL_IO_EXTENTS];
            for (index, run) in runs.iter().enumerate() {
                let blocks = u32::try_from(run.bytes / block_size as usize).map_err(|_| {
                    Ext4Error::new(EINVAL as _, "physical I/O block count overflow")
                })?;
                extents[index] = PhysicalIoExtent {
                    file_offset: run.file_offset,
                    pblock: run.pblock,
                    physical_block_id: 0,
                    bytes: run.bytes,
                    blocks,
                    segment_start: 0,
                    segment_count: 0,
                };
            }
            Ok(Some((
                logical_bytes,
                io_bytes,
                runs.len(),
                extents,
                inode.mapping_seq,
            )))
        })?
        .map(
            |(logical_bytes, io_bytes, extent_count, mut extents, mapping_seq)| {
                for extent in extents.iter_mut().take(extent_count) {
                    extent.physical_block_id = self.bdev.direct_physical_block_id(extent.pblock);
                }
                PhysicalIoPlan {
                    ino,
                    filesystem_id: self.filesystem_id,
                    file_offset: offset,
                    logical_bytes,
                    io_bytes,
                    operation: PhysicalIoOperation::Read,
                    extent_count,
                    extents,
                    segment_count: 0,
                    segments: [PhysicalIoSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_IO_SEGMENTS],
                    mapping_seq,
                }
            },
        )
        .map_or(Ok(None), |plan| Ok(Some(plan)))
    }

    /// Performs one complete typed allocated-extent scan while the filesystem lock is
    /// held by the caller.  The inode cache is allowed to service the mapping
    /// walk, but no Linux userspace representation or pointer enters this
    /// layer.
    pub fn map_extents(
        &mut self,
        ino: u32,
        start: u64,
        length: u64,
        max_extents: usize,
    ) -> Ext4Result<crate::ExtentMap> {
        self.with_cached_inode_ref(ino, |inode| inode.map_extents(start, length, max_extents))
    }

    /// Prepares an owned physical direct-I/O plan.  All filesystem eligibility
    /// and SG normalization happens before this returns; no queue slot,
    /// descriptor, cache invalidation, or metadata mutation is performed.
    ///
    /// The returned plan owns only physical numbers and fixed-size metadata,
    /// so it is safe to move to a worker while the filesystem lock is
    /// released.  The caller's pin owner remains outside this crate and must
    /// outlive publication and exact completion retirement.
    pub fn prepare_physical_io_plan(
        &mut self,
        ino: u32,
        operation: PhysicalIoOperation,
        offset: u64,
        len: usize,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<Option<PhysicalIoPlan>> {
        let Some(mut plan) = self.plan_physical_io(ino, offset, len, operation.is_write())? else {
            return Ok(None);
        };
        plan.operation = operation;
        if !copy_physical_segments_into_plan(&mut plan, segments) {
            return Ok(None);
        }
        Ok(Some(plan))
    }

    /// Short alias for callers which already use the prepare/publish/finalize
    /// vocabulary.
    pub fn prepare_physical_io(
        &mut self,
        ino: u32,
        operation: PhysicalIoOperation,
        offset: u64,
        len: usize,
        segments: &[PhysicalIoSegment],
    ) -> Ext4Result<Option<PhysicalIoPlan>> {
        self.prepare_physical_io_plan(ino, operation, offset, len, segments)
    }

    /// Revalidates a plan after a completed synchronous device operation.
    /// Mapping changes are terminal: callers must not bounce or retry after
    /// an accepted physical operation.
    pub fn validate_physical_io_plan(&mut self, plan: PhysicalIoPlan) -> Ext4Result<()> {
        let filesystem_id = self.filesystem_id;
        let valid = self.with_inode_ref_mut(plan.ino, |inode| {
            if filesystem_id != plan.filesystem_id || inode.mapping_seq != plan.mapping_seq {
                return Ok(false);
            }
            let Some(runs) =
                inode.map_iomap_runs_without_cache(plan.file_offset, plan.io_bytes, false)?
            else {
                return Ok(false);
            };
            if runs.len() != plan.extent_count {
                return Ok(false);
            }
            Ok(runs.iter().enumerate().all(|(index, run)| {
                let Some(extent) = plan.extent(index) else {
                    return false;
                };
                run.seq == plan.mapping_seq
                    && run.kind == MappedRunKind::Written
                    && run.pblock == extent.pblock
                    && run.file_offset == extent.file_offset
                    && run.bytes == extent.bytes
            }))
        })?;
        if valid {
            Ok(())
        } else {
            Err(Ext4Error::new(EIO as _, "stale physical I/O mapping"))
        }
    }

    /// Finalizes a prepared plan after the upper effect owner has established
    /// that every exact device completion was successful.  A false completion
    /// proof is terminal and never selects a synchronous fallback.
    pub fn finalize_physical_io_plan(
        &mut self,
        plan: PhysicalIoPlan,
        all_completions_success: bool,
    ) -> Ext4Result<()> {
        if !all_completions_success {
            return Err(Ext4Error::new(EIO as _, "physical I/O completion failed"));
        }
        match plan.operation {
            PhysicalIoOperation::Read => self.validate_physical_io_plan(plan),
            PhysicalIoOperation::Write => self.commit_physical_io_write(plan),
        }
    }

    /// Commits cache invalidation for a completed physical overwrite after
    /// revalidating that the mapped extent did not change while the device
    /// was running.
    pub fn commit_physical_io_write(&mut self, plan: PhysicalIoPlan) -> Ext4Result<()> {
        if plan.logical_bytes != plan.io_bytes {
            return Err(Ext4Error::new(
                EINVAL as _,
                "physical overwrite plan crosses EOF",
            ));
        }
        self.validate_physical_io_plan(plan)?;
        for index in 0..plan.extent_count {
            let extent = plan
                .extent(index)
                .expect("physical plan extent count exceeds fixed capacity");
            self.bdev
                .invalidate_logical_block_range(extent.pblock, extent.blocks);
        }
        Ok(())
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
        let Some((to_be_read, _block_size, runs)) =
            self.mapped_aligned_read_plan(ino, offset, len)?
        else {
            return Ok(None);
        };
        if to_be_read == 0 {
            return Ok(Some(0));
        }
        if !runs_align_segments(&runs, bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }
        if !segments_are_device_block_sized(bufs.iter().map(|buf| buf.len())) {
            return Ok(None);
        }

        let mut segment = 0usize;
        let mut total = 0usize;
        let mut completed_runs = 0usize;
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
            let read = match self
                .bdev
                .dev_mut()
                .read_blocks_vectored(block_id, &mut bufs[start..segment])
            {
                Ok(read) => read,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if read > run.bytes {
                if total != 0 {
                    break;
                }
                return Err(Ext4Error::new(
                    EIO as _,
                    "mapped read exceeded the planned run",
                ));
            }
            total += read;
            if read < run.bytes {
                break;
            }
            completed_runs += 1;
        }
        record_mapped_read_vectored(completed_runs, total);
        Ok(Some(total))
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
        let completed_runs = runs
            .iter()
            .scan(0usize, |total, run| {
                *total = total.checked_add(run.bytes)?;
                Some(*total)
            })
            .take_while(|end| *end <= submission.bytes)
            .count();
        record_mapped_read_vectored(completed_runs, submission.bytes);
        Ok(Some(submission))
    }
    pub fn write_at(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.ensure_metadata_writable()?;
        self.with_cached_inode_ref(ino, |inode| {
            if inode.is_immutable() || (inode.is_append_only() && offset != inode.size()) {
                return Err(Ext4Error::new(
                    EPERM as _,
                    "immutable or append-only ext4 inode",
                ));
            }
            inode.write_at(buf, offset)
        })
    }
    pub fn write_at_aligned_hot(&mut self, ino: u32, buf: &[u8], offset: u64) -> Ext4Result<usize> {
        self.ensure_metadata_writable()?;
        self.with_cached_inode_ref(ino, |inode| {
            if inode.is_immutable() || (inode.is_append_only() && offset != inode.size()) {
                return Err(Ext4Error::new(
                    EPERM as _,
                    "immutable or append-only ext4 inode",
                ));
            }
            inode.write_at_aligned_hot(buf, offset)
        })
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
            if inode.is_immutable() || (inode.is_append_only() && offset != inode.size()) {
                return Err(Ext4Error::new(
                    EPERM as _,
                    "immutable or append-only ext4 inode",
                ));
            }
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
        let mut total = 0usize;
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
            let written = match self
                .bdev
                .dev_mut()
                .write_blocks_vectored(block_id, &bufs[start..segment])
            {
                Ok(written) => written,
                Err(_) if total != 0 => break,
                Err(error) => return Err(error),
            };
            if written > run.bytes {
                if total != 0 {
                    break;
                }
                return Err(Ext4Error::new(
                    EIO as _,
                    "mapped write exceeded the planned run",
                ));
            }
            let invalidated_blocks = written / block_size as usize;
            if invalidated_blocks != 0 {
                self.bdev
                    .invalidate_logical_block_range(run.pblock, invalidated_blocks as u32);
            }
            total += written;
            if written < run.bytes {
                break;
            }
        }
        record_mapped_overwrite_vectored_hit(total);
        Ok(Some(total))
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
            if inode.is_immutable() || (inode.is_append_only() && offset != inode.size()) {
                return Err(Ext4Error::new(
                    EPERM as _,
                    "immutable or append-only ext4 inode",
                ));
            }
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
        if runs.len() != 1 {
            return Ok(None);
        }
        let run = &runs[0];
        if run.bytes != len {
            return Ok(None);
        }
        let block_id = self.bdev.direct_physical_block_id(run.pblock);
        let Some(submission) = self
            .bdev
            .dev_mut()
            .try_write_blocks_vectored_async_submit(block_id, bufs)?
        else {
            return Ok(None);
        };
        self.bdev.invalidate_logical_block_range(
            run.pblock,
            (submission.bytes / block_size as usize) as u32,
        );
        record_mapped_overwrite_vectored_hit(submission.bytes);
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
        if inode.is_immutable() || inode.is_append_only() {
            let error = Ext4Error::new(EPERM as _, "immutable or append-only ext4 inode");
            return combine_operation_and_release(Err(error), inode.finish());
        }
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
    /// Native extent allocation used by the VFS typed fallocate bridge.
    pub fn allocate_range(
        &mut self,
        ino: u32,
        offset: u64,
        len: u64,
        keep_size: bool,
    ) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let operation = inode.allocate_range(offset, len, keep_size);
        let operation = combine_operation_and_release(operation, inode.finish());
        let operation = combine_operation_and_release(operation, self.invalidate_hot_inode(ino));
        self.observe_metadata_result(operation)
    }
    /// Native PUNCH_HOLE path.  InodeRef calls ext4_extent_remove_space, so
    /// fully-covered extents are split/removed and their blocks freed.
    pub fn punch_hole(&mut self, ino: u32, offset: u64, len: u64) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let operation = inode.punch_hole(offset, len);
        let operation = combine_operation_and_release(operation, inode.finish());
        let operation = combine_operation_and_release(operation, self.invalidate_hot_inode(ino));
        self.observe_metadata_result(operation)
    }
    pub fn zero_range(
        &mut self,
        ino: u32,
        offset: u64,
        len: u64,
        keep_size: bool,
    ) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let operation = inode.zero_range(offset, len, keep_size);
        let operation = combine_operation_and_release(operation, inode.finish());
        let operation = combine_operation_and_release(operation, self.invalidate_hot_inode(ino));
        self.observe_metadata_result(operation)
    }
    pub fn collapse_range(&mut self, ino: u32, offset: u64, len: u64) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let operation = inode.collapse_range(offset, len);
        let operation = combine_operation_and_release(operation, inode.finish());
        let operation = combine_operation_and_release(operation, self.invalidate_hot_inode(ino));
        self.observe_metadata_result(operation)
    }
    pub fn insert_range(&mut self, ino: u32, offset: u64, len: u64) -> Ext4Result<()> {
        self.ensure_metadata_writable()?;
        self.invalidate_hot_inode(ino)?;
        let mut inode = self.inode_ref(ino)?;
        let operation = inode.insert_range(offset, len);
        let operation = combine_operation_and_release(operation, inode.finish());
        let operation = combine_operation_and_release(operation, self.invalidate_hot_inode(ino));
        self.observe_metadata_result(operation)
    }
    pub fn set_symlink(&mut self, _ino: u32, _buf: &[u8]) -> Ext4Result<()> {
        Err(Ext4Error::new(
            ENOTSUP as _,
            "rewriting an ext4 symbolic link is not supported",
        ))
    }
    pub fn lookup(&mut self, parent: u32, name: &[u8]) -> Ext4Result<DirLookupResult<Hal>> {
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
        name: &[u8],
        ty: InodeType,
        mode: u32,
        symlink_target: Option<&[u8]>,
        project_id: Option<u32>,
        user: Option<(u32, u32)>,
        rdev: Option<u64>,
        initial_time: Option<Duration>,
        access_acl: Option<&[u8]>,
        default_acl: Option<&[u8]>,
        project_inherit: bool,
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
            if let Some(project_id) = project_id {
                child.set_project_id(project_id);
            }
            if let Some(rdev) = rdev {
                child.set_rdev(rdev);
            }
            if default_acl.is_some() && ty != InodeType::Directory {
                return Err(Ext4Error::new(EINVAL as _, "default ACL for non-directory"));
            }
            if project_inherit {
                if ty != InodeType::Directory {
                    return Err(Ext4Error::new(
                        EINVAL as _,
                        "project inheritance for non-directory",
                    ));
                }
                child.set_flags(child.flags() | 0x2000_0000);
            }
            // These mutations occur before `parent.add_entry`; the JBD scope
            // therefore commits inode flags/xattrs and the only discoverable
            // name together, never as a post-create repair.
            if let Some(access_acl) = access_acl {
                child.set_xattr(b"system.posix_acl_access", access_acl)?;
            }
            if let Some(default_acl) = default_acl {
                child.set_xattr(b"system.posix_acl_default", default_acl)?;
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
                let dot = child.add_entry(b".", &mut self_ref);
                combine_operation_and_release(dot, self_ref.finish())?;
                child.add_entry(b"..", &mut parent)?;
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
        name: &[u8],
        ty: InodeType,
        mode: u32,
        project_id: Option<u32>,
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
        self.create_inner(
            parent,
            name,
            ty,
            mode,
            None,
            project_id,
            user,
            rdev,
            initial_time,
            None,
            None,
            false,
        )
    }

    /// Creates an inode and installs its namespace-visible initial attributes
    /// in the same pre-`add_entry` JBD mutation.  Callers must pass already
    /// validated Linux ACL blobs; this layer deliberately never reparses or
    /// defers them after the name is reachable.
    pub fn create_prepared(
        &mut self,
        parent: u32,
        name: &[u8],
        ty: InodeType,
        mode: u32,
        project_id: Option<u32>,
        project_inherit: bool,
        access_acl: Option<&[u8]>,
        default_acl: Option<&[u8]>,
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
        self.create_inner(
            parent,
            name,
            ty,
            mode,
            None,
            project_id,
            user,
            rdev,
            initial_time,
            access_acl,
            default_acl,
            project_inherit,
        )
    }

    pub fn create_symlink(
        &mut self,
        parent: u32,
        name: &[u8],
        target: &[u8],
        mode: u32,
        project_id: Option<u32>,
        user: Option<(u32, u32)>,
        initial_time: Option<Duration>,
    ) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        self.create_inner(
            parent,
            name,
            InodeType::Symlink,
            mode,
            Some(target),
            project_id,
            user,
            None,
            initial_time,
            None,
            None,
            false,
        )
    }

    pub fn create_symlink_prepared(
        &mut self,
        parent: u32,
        name: &[u8],
        target: &[u8],
        mode: u32,
        project_id: Option<u32>,
        access_acl: Option<&[u8]>,
        default_acl: Option<&[u8]>,
        user: Option<(u32, u32)>,
        initial_time: Option<Duration>,
    ) -> Ext4Result<(InodeToken, Arc<AtomicU64>)> {
        self.create_inner(
            parent,
            name,
            InodeType::Symlink,
            mode,
            Some(target),
            project_id,
            user,
            None,
            initial_time,
            access_acl,
            default_acl,
            false,
        )
    }

    /// Atomically exchanges two ext4 directory entries.  The directory-entry
    /// blocks are dirtied while the same filesystem transaction is active;
    /// link counts are deliberately unchanged because each inode transfers
    /// one existing name rather than gaining or losing a link.
    pub fn rename_exchange(
        &mut self,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
        expected_src: InodeToken,
        expected_dst: InodeToken,
        _timestamp: Option<Duration>,
    ) -> Ext4Result {
        self.ensure_metadata_writable()?;
        self.drain_hot_inodes()?;
        self.reap_pending_unlinked_inodes(NAMESPACE_REAP_BUDGET)?;
        let (src, _) = self.lookup_inode(src_dir, src_name)?;
        let (dst, _) = self.lookup_inode(dst_dir, dst_name)?;
        let src_ref = self.inode_ref(src)?;
        let dst_ref = self.inode_ref(dst)?;
        if src_ref.token() != expected_src || dst_ref.token() != expected_dst {
            let primary = Ext4Error::new(ENOENT as _, "stale ext4 exchange identity");
            return combine_operation_and_release(
                combine_operation_and_release(Err(primary), src_ref.finish()),
                dst_ref.finish(),
            );
        }
        if src == dst {
            let operation = combine_operation_and_release(Ok(()), src_ref.finish());
            return combine_operation_and_release(operation, dst_ref.finish());
        }
        let journal = self.journal_scope()?;
        // Reserve and validate every inode reference before the first block is
        // marked dirty. From this point a writeback failure poisons the
        // filesystem instead of returning a retryable half-exchange.
        let source_is_dir = src_ref.is_dir();
        let destination_is_dir = dst_ref.is_dir();
        let mut source_parent = if src_dir != dst_dir && source_is_dir != destination_is_dir {
            Some(self.inode_ref(src_dir)?)
        } else {
            None
        };
        let mut destination_parent = if src_dir != dst_dir && source_is_dir != destination_is_dir {
            Some(self.inode_ref(dst_dir)?)
        } else {
            None
        };
        // A directory exchanged with a non-directory transfers exactly one
        // parent link. Admit the receiving parent before any dentry block is
        // dirtied; directory↔directory and file↔file exchanges are balanced.
        if src_dir != dst_dir && source_is_dir != destination_is_dir {
            if source_is_dir {
                destination_parent
                    .as_ref()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange destination parent"))?
                    .ensure_can_inc_nlink()?;
            } else {
                source_parent
                    .as_ref()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange source parent"))?
                    .ensure_can_inc_nlink()?;
            }
        }
        let mut source_entry = self.lookup(src_dir, src_name)?;
        let mut destination_entry = self.lookup(dst_dir, dst_name)?;
        let mut source_dotdot = if source_is_dir && src_dir != dst_dir {
            Some(self.lookup(src, b"..")?)
        } else {
            None
        };
        let mut destination_dotdot = if destination_is_dir && src_dir != dst_dir {
            Some(self.lookup(dst, b"..")?)
        } else {
            None
        };
        let operation = (|| {
            source_entry.set_entry_ino(dst)?;
            destination_entry.set_entry_ino(src)?;
            if let Some(dotdot) = source_dotdot.as_mut() {
                dotdot.set_entry_ino(dst_dir)?;
            }
            if let Some(dotdot) = destination_dotdot.as_mut() {
                dotdot.set_entry_ino(src_dir)?;
            }
            if source_is_dir && !destination_is_dir {
                source_parent
                    .as_mut()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange source parent"))?
                    .dec_parent_dir_nlink();
                destination_parent
                    .as_mut()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange destination parent"))?
                    .inc_nlink();
            } else if destination_is_dir && !source_is_dir {
                destination_parent
                    .as_mut()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange destination parent"))?
                    .dec_parent_dir_nlink();
                source_parent
                    .as_mut()
                    .ok_or_else(|| Ext4Error::new(EIO as _, "missing exchange source parent"))?
                    .inc_nlink();
            }
            Ok(())
        })();
        let operation = combine_operation_and_release(operation, source_entry.finish());
        let operation = combine_operation_and_release(operation, destination_entry.finish());
        let operation = match source_dotdot {
            Some(dotdot) => combine_operation_and_release(operation, dotdot.finish()),
            None => operation,
        };
        let operation = match destination_dotdot {
            Some(dotdot) => combine_operation_and_release(operation, dotdot.finish()),
            None => operation,
        };
        let operation = match source_parent {
            Some(parent) => combine_operation_and_release(operation, parent.finish()),
            None => operation,
        };
        let operation = match destination_parent {
            Some(parent) => combine_operation_and_release(operation, parent.finish()),
            None => operation,
        };
        let operation = combine_operation_and_release(operation, src_ref.finish());
        let operation = combine_operation_and_release(operation, dst_ref.finish());
        let operation = if operation.is_ok() {
            combine_operation_and_release(operation, journal.commit())
        } else {
            drop(journal);
            operation
        };
        if operation.is_err() {
            self.metadata_poisoned = true;
        }
        self.observe_metadata_result(operation)
    }

    /// Moves `src` and installs ext4's native whiteout representation
    /// (character device 0:0) at the old name.  The whiteout is fully
    /// allocated and linked under a private staging name *before* the
    /// destination can be replaced; exchange then installs it at the source
    /// name.  Thus whiteout allocation failure cannot destroy an existing
    /// replacement target.
    pub fn rename_whiteout(
        &mut self,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
        expected_src: InodeToken,
        expected_dst: Option<InodeToken>,
        timestamp: Option<Duration>,
    ) -> Ext4Result {
        let staging = loop {
            let name = whiteout_staging_name();
            match self.lookup_inode(src_dir, &name) {
                Err(error) if error.code == ENOENT as i32 && !error.metadata_may_have_changed() => {
                    break name;
                }
                Ok(_) => continue,
                Err(error) => return Err(error),
            }
        };
        let journal = self.journal_scope()?;
        let (whiteout, _) = self.create(
            src_dir,
            &staging,
            InodeType::CharacterDevice,
            0,
            None,
            None,
            Some(0),
            timestamp,
        )?;
        let exchanged = self.rename_exchange(
            src_dir,
            src_name,
            src_dir,
            &staging,
            expected_src,
            whiteout,
            timestamp,
        );
        if let Err(primary) = exchanged {
            // The staging inode is not user-visible by construction. Remove
            // it only when the failed exchange proved no metadata changed;
            // otherwise preserve state and poison the filesystem.
            if primary.metadata_may_have_changed() {
                self.metadata_poisoned = true;
            } else {
                let _ = self.unlink_checked(src_dir, &staging, Some(whiteout), Some(false), None);
            }
            self.release_inode_handle(whiteout);
            drop(journal);
            return Err(primary);
        }
        self.release_inode_handle(whiteout);
        match self.rename(
            src_dir,
            &staging,
            dst_dir,
            dst_name,
            expected_src,
            expected_dst,
            timestamp,
        ) {
            Ok(()) => {
                let committed = journal.commit();
                if committed.is_err() {
                    self.metadata_poisoned = true;
                }
                self.observe_metadata_result(committed)
            }
            Err(primary) => {
                // A pre-commit destination failure can restore the exact
                // source/whiteout pair. Once the lower rename reports a
                // metadata-changing error it owns an ambiguous journal state,
                // so fail closed instead of risking a destructive rollback.
                if primary.metadata_may_have_changed() {
                    self.metadata_poisoned = true;
                } else {
                    let rollback = self.rename_exchange(
                        src_dir,
                        &staging,
                        src_dir,
                        src_name,
                        expected_src,
                        whiteout,
                        timestamp,
                    );
                    match rollback {
                        Ok(()) => {
                            // The rollback moved the staging whiteout back to
                            // its private name. Remove it before returning so
                            // a failed syscall has no synthetic visible entry.
                            if self
                                .unlink_checked(
                                    src_dir,
                                    &staging,
                                    Some(whiteout),
                                    Some(false),
                                    None,
                                )
                                .is_err()
                            {
                                self.metadata_poisoned = true;
                            }
                        }
                        Err(_) => self.metadata_poisoned = true,
                    }
                }
                drop(journal);
                Err(primary)
            }
        }
    }

    pub fn rename(
        &mut self,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
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
                let mut result = self.clone_ref(&src_ref)?.lookup(b"..")?;
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
                        let mut result = self.clone_ref(&src_ref)?.lookup(b"..")?;
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
        name: &[u8],
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

    pub fn unlink(&mut self, dir: u32, name: &[u8]) -> Ext4Result {
        self.unlink_checked(dir, name, None, None, None)
    }

    /// Removes one directory entry after validating its stable identity and
    /// type. A VFS caller may supply one timestamp for the committed unlink;
    /// internal rename and rollback callers pass `None` to retain their own
    /// timestamp policy.
    pub fn unlink_checked(
        &mut self,
        dir: u32,
        name: &[u8],
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
    use alloc::vec;

    use super::*;

    fn timestamp_test_inode(fs: &mut ext4_fs, inode: &mut ext4_inode) -> InodeRef<DummyHal> {
        let mut reference = InodeRef::try_uninitialized().unwrap();
        reference.inner.fs = fs;
        reference.inner.inode = inode;
        reference
    }

    #[test]
    fn physical_plan_rejects_partial_block_eof_without_tail_bounce() {
        // The request covers the final 2 KiB of a 10 KiB file and has a full
        // 4 KiB caller buffer. Reading the complete final block would clobber
        // bytes after the returned short count, so physical preflight must
        // fall back instead of publishing a zero-copy request.
        assert_eq!(
            physical_io_read_lengths(10 * 1024, 8 * 1024, 4 * 1024, 4096, false),
            None
        );
        assert_eq!(
            physical_io_read_lengths(12 * 1024, 8 * 1024, 8 * 1024, 4096, false),
            Some((4 * 1024, 4 * 1024))
        );
        assert_eq!(
            physical_io_read_lengths(10 * 1024, 8 * 1024, 4 * 1024, 4096, true),
            None
        );
    }

    fn physical_effect_test_plan(operation: PhysicalIoOperation) -> PhysicalIoPlan {
        let mut extents = [PhysicalIoExtent {
            file_offset: 0,
            pblock: 0,
            physical_block_id: 0,
            bytes: 0,
            blocks: 0,
            segment_start: 0,
            segment_count: 0,
        }; MAX_PHYSICAL_IO_EXTENTS];
        extents[0] = PhysicalIoExtent {
            file_offset: 0,
            pblock: 10,
            physical_block_id: 100,
            bytes: 8 * 1024,
            blocks: 2,
            segment_start: 0,
            segment_count: 0,
        };
        extents[1] = PhysicalIoExtent {
            file_offset: 8 * 1024,
            pblock: 20,
            physical_block_id: 200,
            bytes: 4 * 1024,
            blocks: 1,
            segment_start: 0,
            segment_count: 0,
        };
        let mut plan = PhysicalIoPlan {
            ino: 17,
            filesystem_id: 9,
            file_offset: 0,
            logical_bytes: 12 * 1024,
            io_bytes: 12 * 1024,
            operation,
            extent_count: 2,
            extents,
            segment_count: 0,
            segments: [PhysicalIoSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_IO_SEGMENTS],
            mapping_seq: 31,
        };
        assert!(copy_physical_segments_into_plan(
            &mut plan,
            &[
                PhysicalIoSegment {
                    paddr: 0x1000,
                    len: 4 * 1024,
                },
                PhysicalIoSegment {
                    paddr: 0x3000,
                    len: 8 * 1024,
                },
            ],
        ));
        assert_eq!(plan.segment_count(), 3);
        assert_eq!(plan.extent(0).unwrap().segment_start(), 0);
        assert_eq!(plan.extent(0).unwrap().segment_count(), 2);
        assert_eq!(plan.extent(1).unwrap().segment_start(), 2);
        assert_eq!(plan.extent(1).unwrap().segment_count(), 1);
        plan
    }

    fn physical_effect_max_extent_test_plan(operation: PhysicalIoOperation) -> PhysicalIoPlan {
        let mut extents = [PhysicalIoExtent {
            file_offset: 0,
            pblock: 0,
            physical_block_id: 0,
            bytes: 0,
            blocks: 0,
            segment_start: 0,
            segment_count: 0,
        }; MAX_PHYSICAL_IO_EXTENTS];
        let mut segments = [PhysicalIoSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_IO_SEGMENTS];
        for index in 0..MAX_PHYSICAL_IO_EXTENTS {
            let file_offset = (index * 4 * 1024) as u64;
            extents[index] = PhysicalIoExtent {
                file_offset,
                pblock: 0x200 + index as u64,
                physical_block_id: 0x1000 + index as u64,
                bytes: 4 * 1024,
                blocks: 1,
                segment_start: 0,
                segment_count: 0,
            };
            segments[index] = PhysicalIoSegment {
                paddr: 0x10_0000 + index * 4 * 1024,
                len: 4 * 1024,
            };
        }
        let mut plan = PhysicalIoPlan {
            ino: 17,
            filesystem_id: 9,
            file_offset: 0,
            logical_bytes: MAX_PHYSICAL_IO_EXTENTS * 4 * 1024,
            io_bytes: MAX_PHYSICAL_IO_EXTENTS * 4 * 1024,
            operation,
            extent_count: MAX_PHYSICAL_IO_EXTENTS,
            extents,
            segment_count: 0,
            segments,
            mapping_seq: 31,
        };
        assert!(copy_physical_segments_into_plan(
            &mut plan,
            &segments[..MAX_PHYSICAL_IO_EXTENTS],
        ));
        assert_eq!(plan.segment_count(), MAX_PHYSICAL_IO_EXTENTS);
        plan
    }

    #[test]
    fn physical_plan_bounds_the_union_of_extent_and_source_sg_boundaries() {
        let mut extents = [PhysicalIoExtent {
            file_offset: 0,
            pblock: 0,
            physical_block_id: 0,
            bytes: 0,
            blocks: 0,
            segment_start: 0,
            segment_count: 0,
        }; MAX_PHYSICAL_IO_EXTENTS];
        for (index, extent) in extents.iter_mut().enumerate() {
            *extent = PhysicalIoExtent {
                file_offset: (index * 8 * 1024) as u64,
                pblock: 0x200 + (index * 2) as u64,
                physical_block_id: 0x1000 + (index * 16) as u64,
                bytes: 8 * 1024,
                blocks: 2,
                segment_start: 0,
                segment_count: 0,
            };
        }
        let mut plan = PhysicalIoPlan {
            ino: 23,
            filesystem_id: 11,
            file_offset: 0,
            logical_bytes: 128 * 1024,
            io_bytes: 128 * 1024,
            operation: PhysicalIoOperation::Read,
            extent_count: MAX_PHYSICAL_IO_EXTENTS,
            extents,
            segment_count: 0,
            segments: [PhysicalIoSegment { paddr: 0, len: 0 }; MAX_PHYSICAL_IO_SEGMENTS],
            mapping_seq: 37,
        };
        let mut input = [PhysicalIoSegment {
            paddr: 0,
            len: 8 * 1024,
        }; MAX_PHYSICAL_IO_INPUT_SEGMENTS];
        input[0].len = 4 * 1024;
        input[MAX_PHYSICAL_IO_INPUT_SEGMENTS - 1].len = 12 * 1024;
        for (index, segment) in input.iter_mut().enumerate() {
            segment.paddr = 0x20_0000 + index * 0x20_000;
        }

        assert!(copy_physical_segments_into_plan(&mut plan, &input));
        assert_eq!(plan.segment_count(), MAX_PHYSICAL_IO_SEGMENTS);
        let extent_segment_counts = plan.extents[..plan.extent_count]
            .iter()
            .copied()
            .map(PhysicalIoExtent::segment_count)
            .collect::<Vec<_>>();
        assert_eq!(
            extent_segment_counts.iter().copied().sum::<usize>(),
            MAX_PHYSICAL_IO_SEGMENTS
        );
        assert_eq!(
            extent_segment_counts
                .iter()
                .filter(|&&count| count == 2)
                .count(),
            MAX_PHYSICAL_IO_EXTENTS - 1
        );
        assert_eq!(
            extent_segment_counts
                .iter()
                .filter(|&&count| count == 1)
                .count(),
            1
        );
    }

    #[test]
    fn physical_effect_owns_extent_sliced_sg_and_publishes_one_exact_batch() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Write);
        let mut effect = PhysicalIoEffect::new(plan);
        let mut submitted = Vec::new();
        let outcome = unsafe {
            effect.publish_with(|requests| {
                submitted.extend_from_slice(requests);
                Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                    PhysicalIoBatchSubmission {
                        handles: vec![41, 42],
                        cookies: vec![401, 402],
                        bytes: 12 * 1024,
                        submitted: 2,
                        terminal: false,
                    },
                ))
            })
        }
        .unwrap();

        assert_eq!(submitted.len(), 2);
        assert_eq!(submitted[0].block_id, 100);
        assert_eq!(submitted[0].operation, PhysicalIoOperation::Write);
        assert_eq!(
            submitted[0].physical_segments(),
            plan.segments().get(..2).unwrap()
        );
        assert_eq!(submitted[1].block_id, 200);
        assert_eq!(
            submitted[1].physical_segments(),
            plan.segments().get(2..3).unwrap()
        );
        assert_eq!(
            outcome,
            PhysicalIoPublishOutcome::Published(effect.publication().unwrap())
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Published);
    }

    #[test]
    fn physical_effect_publishes_max_extent_batch_and_settles_once_out_of_order() {
        let plan = physical_effect_max_extent_test_plan(PhysicalIoOperation::Read);
        assert_eq!(plan.extent_count(), MAX_PHYSICAL_IO_EXTENTS);
        let mut effect = PhysicalIoEffect::new(plan);
        let mut submitted = [PhysicalIoBatchRequest::empty(); MAX_PHYSICAL_IO_EXTENTS];
        let mut handles = Vec::with_capacity(MAX_PHYSICAL_IO_EXTENTS);
        let mut cookies = Vec::with_capacity(MAX_PHYSICAL_IO_EXTENTS);
        for index in 0..MAX_PHYSICAL_IO_EXTENTS {
            handles.push(0x1000_0000 + index as u64);
            cookies.push(0x2000_0000 + index as u64);
        }

        let outcome = unsafe {
            effect.publish_with(|requests| {
                assert_eq!(requests.len(), MAX_PHYSICAL_IO_EXTENTS);
                submitted[..requests.len()].copy_from_slice(requests);
                for index in 0..MAX_PHYSICAL_IO_EXTENTS {
                    let extent = plan.extent(index).unwrap();
                    assert_eq!(requests[index].block_id, extent.physical_block_id());
                    assert_eq!(requests[index].bytes, extent.bytes());
                    assert_eq!(
                        requests[index].physical_segments(),
                        plan.segments().get(index..index + 1).unwrap()
                    );
                }
                Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                    PhysicalIoBatchSubmission {
                        handles,
                        cookies,
                        bytes: plan.io_bytes(),
                        submitted: MAX_PHYSICAL_IO_EXTENTS,
                        terminal: false,
                    },
                ))
            })
        }
        .unwrap();
        let publication = match outcome {
            PhysicalIoPublishOutcome::Published(publication) => publication,
            other => panic!("16-extent batch must publish fully: {other:?}"),
        };
        assert_eq!(publication.count(), MAX_PHYSICAL_IO_EXTENTS);
        assert_eq!(publication.bytes(), plan.io_bytes());
        for index in 0..MAX_PHYSICAL_IO_EXTENTS {
            assert_eq!(publication.handle(index), Some(0x1000_0000 + index as u64));
            assert_eq!(publication.cookie(index), Some(0x2000_0000 + index as u64));
            assert_eq!(submitted[index].segment_count, 1);
        }

        const COMPLETION_ORDER: [usize; MAX_PHYSICAL_IO_EXTENTS] =
            [15, 3, 12, 0, 9, 6, 14, 1, 10, 5, 8, 2, 13, 4, 11, 7];
        for index in COMPLETION_ORDER {
            assert_eq!(
                effect.record_completion(PhysicalIoCompletion {
                    handle: publication.handle(index).unwrap(),
                    cookie: publication.cookie(index).unwrap(),
                    bytes: plan.extent(index).unwrap().bytes(),
                    success: true,
                }),
                PhysicalIoCompletionOutcome::Accepted
            );
        }
        assert_eq!(effect.state(), PhysicalIoEffectState::Published);
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Settled {
                plan,
                success: true,
            }
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Completed);
    }

    #[test]
    fn physical_effect_queue_full_is_zero_publication_and_keeps_fallback_state() {
        let mut effect =
            PhysicalIoEffect::new(physical_effect_test_plan(PhysicalIoOperation::Read));
        let outcome = unsafe {
            effect.publish_with(|requests| {
                assert_eq!(requests.len(), 2);
                Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                    PhysicalIoNotSubmittedReason::Backpressure,
                ))
            })
        }
        .unwrap();

        assert_eq!(
            outcome,
            PhysicalIoPublishOutcome::NotSubmitted(PhysicalIoNotSubmittedReason::Backpressure)
        );
        assert_eq!(effect.publication(), None);
        assert_eq!(effect.state(), PhysicalIoEffectState::Prepared);
    }

    #[test]
    fn physical_effect_backpressure_retry_reuses_prepared_all_or_none_plan() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        let mut attempts = 0;
        let first = unsafe {
            effect
                .publish_with(|requests| {
                    attempts += 1;
                    assert_eq!(requests.len(), MAX_PHYSICAL_IO_EXTENTS.min(2));
                    Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(
                        PhysicalIoNotSubmittedReason::Backpressure,
                    ))
                })
                .unwrap()
        };
        assert_eq!(
            first,
            PhysicalIoPublishOutcome::NotSubmitted(PhysicalIoNotSubmittedReason::Backpressure)
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Prepared);
        assert_eq!(effect.publication(), None);

        let second = unsafe {
            effect
                .publish_with(|requests| {
                    attempts += 1;
                    assert_eq!(requests.len(), 2);
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: plan.io_bytes(),
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap()
        };
        assert!(matches!(second, PhysicalIoPublishOutcome::Published(_)));
        assert_eq!(attempts, 2);
        assert_eq!(effect.state(), PhysicalIoEffectState::Published);
    }

    #[test]
    fn physical_effect_permanent_not_submitted_reasons_remain_distinct() {
        for reason in [
            PhysicalIoNotSubmittedReason::Unsupported,
            PhysicalIoNotSubmittedReason::NoMemory,
            PhysicalIoNotSubmittedReason::Invalid,
        ] {
            let mut effect =
                PhysicalIoEffect::new(physical_effect_test_plan(PhysicalIoOperation::Read));
            let outcome = unsafe {
                effect
                    .publish_with(|_| Ok(PhysicalIoBatchSubmitOutcome::NotSubmitted(reason)))
                    .unwrap()
            };
            assert_eq!(outcome, PhysicalIoPublishOutcome::NotSubmitted(reason));
            assert_eq!(effect.state(), PhysicalIoEffectState::Prepared);
            assert_eq!(effect.publication(), None);
        }
    }

    #[test]
    fn physical_effect_partial_publication_is_terminal_and_retains_prefix_handles() {
        let mut effect =
            PhysicalIoEffect::new(physical_effect_test_plan(PhysicalIoOperation::Read));
        let outcome = unsafe {
            effect.publish_with(|_| {
                Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                    PhysicalIoBatchSubmission {
                        handles: vec![77],
                        cookies: vec![707],
                        bytes: 8 * 1024,
                        submitted: 1,
                        terminal: true,
                    },
                ))
            })
        }
        .unwrap();

        let PhysicalIoPublishOutcome::Terminal(publication) = outcome else {
            panic!("partial publication must close the fallback path");
        };
        assert_eq!(publication.count(), 1);
        assert_eq!(publication.handle(0), Some(77));
        assert_eq!(publication.bytes(), 8 * 1024);
        assert!(publication.terminal());
        assert_eq!(effect.state(), PhysicalIoEffectState::Terminal);
    }

    #[test]
    fn physical_effect_malformed_handle_report_is_terminal_not_fallback() {
        let mut effect =
            PhysicalIoEffect::new(physical_effect_test_plan(PhysicalIoOperation::Read));
        let outcome = unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![88],
                            cookies: vec![808],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap()
        };

        let PhysicalIoPublishOutcome::Terminal(publication) = outcome else {
            panic!("malformed accepted prefix must be terminal");
        };
        assert_eq!(publication.count(), 1);
        assert_eq!(publication.handle(0), Some(88));
        assert!(publication.terminal());
        assert_eq!(effect.state(), PhysicalIoEffectState::Terminal);
    }

    #[test]
    fn physical_effect_malformed_report_never_becomes_drop_safe_after_prefix_retirement() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![88],
                            cookies: vec![808],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 88,
                cookie: 808,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Retain(PhysicalIoPendingReason::MalformedPublication)
        );
        assert_ne!(effect.state(), PhysicalIoEffectState::Finalized);
    }

    #[test]
    fn physical_effect_requires_all_exact_successful_completions_before_finalize() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }

        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 22,
                cookie: 2002,
                bytes: 4 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Published);
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 1001,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Published);
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Settled {
                plan,
                success: true,
            }
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::Completed);
        assert_eq!(effect.mark_finalized().unwrap(), plan);
        assert_eq!(effect.state(), PhysicalIoEffectState::Finalized);
    }

    #[test]
    fn physical_effect_terminal_prefix_settles_failure_after_exact_retirement() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![77],
                            cookies: vec![707],
                            bytes: 8 * 1024,
                            submitted: 1,
                            terminal: true,
                        },
                    ))
                })
                .unwrap();
        }
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 77,
                cookie: 707,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Settled {
                plan,
                success: false,
            }
        );
        assert_eq!(effect.state(), PhysicalIoEffectState::SettledFailure);
        assert_eq!(effect.mark_finalized().unwrap(), plan);
    }

    #[test]
    fn physical_effect_wrong_cookie_retains_until_exact_completion_then_fails_closed() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 9999,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::CookieMismatch)
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Retain(PhysicalIoPendingReason::CookieMismatch)
        );
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 1001,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 22,
                cookie: 2002,
                bytes: 4 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Retain(PhysicalIoPendingReason::CookieMismatch)
        );
    }

    #[test]
    fn physical_effect_zero_identity_is_quarantined_before_owner_release() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 0,
                bytes: 8 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::CookieMismatch)
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Retain(PhysicalIoPendingReason::CookieMismatch)
        );
    }

    #[test]
    fn physical_effect_duplicate_completion_is_quarantined() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        let first = PhysicalIoCompletion {
            handle: 11,
            cookie: 1001,
            bytes: 8 * 1024,
            success: true,
        };
        assert_eq!(
            effect.record_completion(first),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.record_completion(first),
            PhysicalIoCompletionOutcome::Retain(PhysicalIoPendingReason::DuplicateCompletion)
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Retain(PhysicalIoPendingReason::DuplicateCompletion)
        );
    }

    #[test]
    fn physical_effect_device_error_is_retired_but_settles_failure() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Read);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 1001,
                bytes: 0,
                success: false,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 22,
                cookie: 2002,
                bytes: 4 * 1024,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        );
        assert_eq!(
            effect.settle(),
            PhysicalIoSettlement::Settled {
                plan,
                success: false,
            }
        );
    }

    #[test]
    fn physical_effect_successful_write_accepts_zero_used_bytes() {
        let plan = physical_effect_test_plan(PhysicalIoOperation::Write);
        let mut effect = PhysicalIoEffect::new(plan);
        unsafe {
            effect
                .publish_with(|_| {
                    Ok(PhysicalIoBatchSubmitOutcome::Submitted(
                        PhysicalIoBatchSubmission {
                            handles: vec![11, 22],
                            cookies: vec![1001, 2002],
                            bytes: 12 * 1024,
                            submitted: 2,
                            terminal: false,
                        },
                    ))
                })
                .unwrap();
        }
        assert!(matches!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 11,
                cookie: 1001,
                bytes: 0,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        ));
        assert!(matches!(
            effect.record_completion(PhysicalIoCompletion {
                handle: 22,
                cookie: 2002,
                bytes: 0,
                success: true,
            }),
            PhysicalIoCompletionOutcome::Accepted
        ));
        assert!(matches!(
            effect.settle(),
            PhysicalIoSettlement::Settled { success: true, .. }
        ));
    }

    #[test]
    fn physical_effect_drop_before_publish_has_no_device_callback() {
        let callback_count = core::cell::Cell::new(0usize);
        {
            let _effect =
                PhysicalIoEffect::new(physical_effect_test_plan(PhysicalIoOperation::Read));
        }
        assert_eq!(callback_count.get(), 0);
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
