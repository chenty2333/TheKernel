use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ffi::c_int,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axtask::current;
use flatten_objects::FlattenObjects;
use linux_raw_sys::general::RLIMIT_NOFILE;
use spin::{Mutex, Once, RwLock, RwLockReadGuard, RwLockWriteGuard};
use starry_process::Pid;

use super::{
    desc::{DescriptionResource, FileDescription, FileDescriptor, FileHandle},
    executable::ExecutableKey,
    flock,
    types::FileLike,
};
use crate::task::{AX_FILE_LIMIT, AsThread};

static NEXT_FD_TABLE_ID: AtomicU64 = AtomicU64::new(1);
static FD_SCOPE_DEFAULT: Once<Arc<FdTable>> = Once::new();

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct FdTableId(u64);

impl FdTableId {
    fn allocate() -> AxResult<Self> {
        NEXT_FD_TABLE_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map(Self)
            .map_err(|_| AxError::TooManyOpenFiles)
    }
}

/// Linux `files_struct` equivalent.
///
/// Its stable identity is shared by `CLONE_FILES` and replaced by ordinary
/// fork/unshare.  Keeping the entries lock inside this type also gives fd
/// lifecycle observers one place to linearize against close and replace.
pub struct FdTable {
    id: FdTableId,
    dnotify_cleanup: Mutex<Option<Box<super::dnotify::TableCleanupWork>>>,
    reservations: Mutex<FdReservations>,
    entries: RwLock<FlattenObjects<FileDescriptor, AX_FILE_LIMIT>>,
}

const FD_RESERVATION_WORDS: usize = AX_FILE_LIMIT.div_ceil(u64::BITS as usize);

struct FdReservations {
    words: [u64; FD_RESERVATION_WORDS],
}

impl FdReservations {
    const fn new() -> Self {
        Self {
            words: [0; FD_RESERVATION_WORDS],
        }
    }

    fn contains(&self, fd: usize) -> bool {
        fd < AX_FILE_LIMIT
            && self.words[fd / u64::BITS as usize] & (1 << (fd % u64::BITS as usize)) != 0
    }

    fn insert(&mut self, fd: usize) {
        self.words[fd / u64::BITS as usize] |= 1 << (fd % u64::BITS as usize);
    }

    fn remove(&mut self, fd: usize) {
        self.words[fd / u64::BITS as usize] &= !(1 << (fd % u64::BITS as usize));
    }
}

/// Linux-style fd-number reservation. The slot is unavailable to concurrent
/// open/dup operations but remains invisible to lookup until the caller has
/// finished constructing the open file description and publishes it.
pub(crate) struct ReservedFd {
    table: Arc<FdTable>,
    table_id: FdTableId,
    fd: c_int,
    cloexec: bool,
    reserved: bool,
}

/// Fallibly reserved storage for descriptors detached under one table lock.
/// Keeping the allocation outside the lock prevents allocator re-entry and
/// makes every subsequent push infallible for the fixed fd-table ceiling.
pub(crate) struct CloseBatch {
    descriptors: Vec<FileDescriptor>,
    dnotify: Vec<super::dnotify::DetachedMark>,
}

impl CloseBatch {
    fn with_capacity(capacity: usize) -> AxResult<Self> {
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        let mut dnotify = Vec::new();
        dnotify
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            descriptors,
            dnotify,
        })
    }

    fn push_removed(
        &mut self,
        descriptor: FileDescriptor,
        dnotify: Option<super::dnotify::DetachedMark>,
    ) {
        self.descriptors.push(descriptor);
        if let Some(dnotify) = dnotify {
            self.dnotify.push(dnotify);
        }
    }

    fn finish_dnotify(&mut self) {
        let detached = core::mem::take(&mut self.dnotify);
        drop(detached);
    }
}

impl ReservedFd {
    pub(crate) const fn fd(&self) -> c_int {
        self.fd
    }

    /// Publishes the fully constructed description into this reserved number.
    /// No allocation or user access occurs under either fd-table lock.
    pub(crate) fn publish(mut self, description: Arc<FileDescription>) -> AxResult<c_int> {
        self.table
            .publish_reserved(self.table_id, self.fd as usize, description, self.cloexec)?;
        self.reserved = false;
        Ok(self.fd)
    }
}

impl Drop for ReservedFd {
    fn drop(&mut self) {
        if self.reserved {
            self.table.release_reservation(self.table_id, self.fd);
        }
    }
}

impl FdTable {
    pub fn new() -> AxResult<Self> {
        Ok(Self {
            id: FdTableId::allocate()?,
            dnotify_cleanup: Mutex::new(None),
            reservations: Mutex::new(FdReservations::new()),
            entries: RwLock::new(FlattenObjects::new()),
        })
    }

    fn from_entries(entries: FlattenObjects<FileDescriptor, AX_FILE_LIMIT>) -> AxResult<Self> {
        Ok(Self {
            id: FdTableId::allocate()?,
            dnotify_cleanup: Mutex::new(None),
            reservations: Mutex::new(FdReservations::new()),
            entries: RwLock::new(entries),
        })
    }

    pub fn fork_copy(&self) -> AxResult<Self> {
        Self::from_entries(self.entries.read().clone())
    }

    pub(crate) const fn id(&self) -> FdTableId {
        self.id
    }

    /// Ensures final table teardown can be published without allocating from
    /// `Drop`. Only tables that have attempted to install dnotify state pay for
    /// this preallocated ownership node.
    pub(crate) fn prepare_dnotify_cleanup(&self) -> AxResult<()> {
        if self.dnotify_cleanup.lock().is_some() {
            return Ok(());
        }
        let work = super::dnotify::TableCleanupWork::try_new(self.id())?;
        let mut slot = self.dnotify_cleanup.lock();
        if slot.is_none() {
            *slot = Some(work);
            return Ok(());
        }
        drop(slot);
        drop(work);
        Ok(())
    }

    pub(crate) fn read(
        &self,
    ) -> RwLockReadGuard<'_, FlattenObjects<FileDescriptor, AX_FILE_LIMIT>> {
        self.entries.read()
    }

    pub(crate) fn write(
        &self,
    ) -> RwLockWriteGuard<'_, FlattenObjects<FileDescriptor, AX_FILE_LIMIT>> {
        self.entries.write()
    }

    /// Revalidates that an fd still names the OFD observed before doing
    /// potentially blocking filesystem metadata work. The callback runs under
    /// only the short table read lock needed to linearize a registry update
    /// against close or replacement.
    pub(crate) fn with_same_description<R>(
        &self,
        fd: c_int,
        expected: super::desc::FileDescriptionId,
        f: impl FnOnce(FdTableId, c_int, &Arc<FileDescription>) -> AxResult<R>,
    ) -> AxResult<R> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        let id = self.id();
        let entries = self.entries.read();
        let description = &entries
            .get(fd as usize)
            .ok_or(AxError::BadFileDescriptor)?
            .description;
        if description.id() != expected {
            return Err(AxError::BadFileDescriptor);
        }
        f(id, fd, description)
    }

    fn release_reservation(&self, table_id: FdTableId, fd: c_int) {
        if self.id != table_id || fd < 0 {
            return;
        }
        let mut reservations = self.reservations.lock();
        reservations.remove(fd as usize);
    }

    fn publish_reserved(
        &self,
        table_id: FdTableId,
        fd: usize,
        description: Arc<FileDescription>,
        cloexec: bool,
    ) -> AxResult<()> {
        if self.id != table_id {
            return Err(AxError::BadState);
        }
        let mut reservations = self.reservations.lock();
        if !reservations.contains(fd) {
            return Err(AxError::BadState);
        }
        let mut entries = self.entries.write();
        entries
            .add_at(
                fd,
                FileDescriptor {
                    description: description.clone(),
                    cloexec,
                },
            )
            .map_err(|_| AxError::BadState)?;
        reservations.remove(fd);
        description.mark_open_committed();
        Ok(())
    }

    pub(crate) fn add_at_least(
        &self,
        description: Arc<FileDescription>,
        min_fd: usize,
        limit: usize,
        cloexec: bool,
    ) -> AxResult<c_int> {
        let reservations = self.reservations.lock();
        let mut entries = self.entries.write();
        let fd = (min_fd..limit.min(AX_FILE_LIMIT))
            .find(|&fd| entries.get(fd).is_none() && !reservations.contains(fd))
            .ok_or(AxError::TooManyOpenFiles)?;
        entries
            .add_at(
                fd,
                FileDescriptor {
                    description: description.clone(),
                    cloexec,
                },
            )
            .map_err(|_| AxError::TooManyOpenFiles)?;
        // Keep the reservation lock until insertion is complete so another
        // thread cannot select the same numeric slot.
        drop(reservations);
        description.mark_open_committed();
        Ok(fd as c_int)
    }

    pub(crate) fn get_description(&self, fd: c_int) -> AxResult<Arc<FileDescription>> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        self.entries
            .read()
            .get(fd as usize)
            .map(|fd| fd.description.clone())
            .ok_or(AxError::BadFileDescriptor)
    }

    pub(crate) fn get_cloexec(&self, fd: c_int) -> AxResult<bool> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        self.entries
            .read()
            .get(fd as usize)
            .map(|fd| fd.cloexec)
            .ok_or(AxError::BadFileDescriptor)
    }

    pub(crate) fn set_cloexec(&self, fd: c_int, cloexec: bool) -> AxResult<()> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        self.entries
            .write()
            .get_mut(fd as usize)
            .ok_or(AxError::BadFileDescriptor)?
            .cloexec = cloexec;
        Ok(())
    }

    pub(crate) fn mark_cloexec_range(&self, first: u32, last: u32) {
        let mut entries = self.entries.write();
        let Some(max_index) = entries.ids().next_back() else {
            return;
        };
        for fd in first..=last.min(max_index as u32) {
            if let Some(descriptor) = entries.get_mut(fd as usize) {
                descriptor.cloexec = true;
            }
        }
    }

    fn remove_locked(
        &self,
        entries: &mut FlattenObjects<FileDescriptor, AX_FILE_LIMIT>,
        fd: usize,
    ) -> Option<(FileDescriptor, Option<super::dnotify::DetachedMark>)> {
        let removed = entries.remove(fd)?;
        let dnotify = crate::file::dnotify::detach_watch(self.id, removed.description.id());
        Some((removed, dnotify))
    }

    fn finish_close(&self, removed: &FileDescriptor) {
        release_posix_locks_on_close(&removed.description);
    }

    pub(crate) fn close(&self, fd: c_int) -> AxResult<FileDescriptor> {
        if fd < 0 {
            return Err(AxError::BadFileDescriptor);
        }
        let (removed, dnotify) = {
            let mut entries = self.entries.write();
            self.remove_locked(&mut entries, fd as usize)
                .ok_or(AxError::BadFileDescriptor)?
        };
        drop(dnotify);
        self.finish_close(&removed);
        Ok(removed)
    }

    /// Closes only the exact OFD still occupying `fd`. This is used to roll
    /// back post-publication initialization without ever closing a numeric fd
    /// that a sibling has already reused.
    pub(crate) fn close_if_same(
        &self,
        fd: c_int,
        expected: super::desc::FileDescriptionId,
    ) -> Option<FileDescriptor> {
        if fd < 0 {
            return None;
        }
        let removed = {
            let mut entries = self.entries.write();
            if entries
                .get(fd as usize)
                .is_none_or(|entry| entry.description.id() != expected)
            {
                return None;
            }
            self.remove_locked(&mut entries, fd as usize)
        };
        let (removed, dnotify) = removed?;
        drop(dnotify);
        self.finish_close(&removed);
        Some(removed)
    }

    pub(crate) fn close_range(&self, first: u32, last: u32) -> AxResult<CloseBatch> {
        let mut removed = CloseBatch::with_capacity(AX_FILE_LIMIT)?;
        {
            let mut entries = self.entries.write();
            let Some(max_index) = entries.ids().next_back() else {
                return Ok(removed);
            };
            for fd in first..=last.min(max_index as u32) {
                if let Some((descriptor, dnotify)) = self.remove_locked(&mut entries, fd as usize) {
                    removed.push_removed(descriptor, dnotify);
                }
            }
        }
        removed.finish_dnotify();
        for descriptor in &removed.descriptors {
            self.finish_close(descriptor);
        }
        Ok(removed)
    }

    pub(crate) fn prepare_cloexec_batch(&self) -> AxResult<CloseBatch> {
        let entries = self.entries.read();
        let count = entries
            .ids()
            .filter(|fd| entries.get(*fd).is_some_and(|entry| entry.cloexec))
            .count();
        drop(entries);
        CloseBatch::with_capacity(count)
    }

    pub(crate) fn close_cloexec(&self, mut removed: CloseBatch) -> CloseBatch {
        {
            let mut entries = self.entries.write();
            for fd in 0..AX_FILE_LIMIT {
                if entries.get(fd).is_some_and(|entry| entry.cloexec)
                    && let Some((descriptor, dnotify)) = self.remove_locked(&mut entries, fd)
                {
                    removed.push_removed(descriptor, dnotify);
                }
            }
        }
        removed.finish_dnotify();
        for descriptor in &removed.descriptors {
            self.finish_close(descriptor);
        }
        removed
    }

    pub(crate) fn close_all(&self) -> AxResult<CloseBatch> {
        let mut removed = CloseBatch::with_capacity(AX_FILE_LIMIT)?;
        {
            let mut entries = self.entries.write();
            for fd in 0..AX_FILE_LIMIT {
                if let Some((descriptor, dnotify)) = self.remove_locked(&mut entries, fd) {
                    removed.push_removed(descriptor, dnotify);
                }
            }
        }
        removed.finish_dnotify();
        for descriptor in &removed.descriptors {
            self.finish_close(descriptor);
        }
        Ok(removed)
    }

    pub(crate) fn dup_replace(
        &self,
        old_fd: c_int,
        new_fd: c_int,
        cloexec: bool,
    ) -> AxResult<Option<FileDescriptor>> {
        if old_fd < 0 || new_fd < 0 || new_fd as usize >= AX_FILE_LIMIT {
            return Err(AxError::BadFileDescriptor);
        }
        let (removed, dnotify) = {
            let reservations = self.reservations.lock();
            if reservations.contains(new_fd as usize) {
                // Linux documents EBUSY for dup2/dup3 racing with fd-number
                // allocation. Never steal a number that SCM_RIGHTS may already
                // have copied to another thread's control buffer.
                return Err(AxError::ResourceBusy);
            }
            let mut entries = self.entries.write();
            let description = entries
                .get(old_fd as usize)
                .map(|descriptor| descriptor.description.clone())
                .ok_or(AxError::BadFileDescriptor)?;
            let replacement = FileDescriptor {
                description: description.clone(),
                cloexec,
            };
            let removed = match entries.add_or_replace_at(new_fd as usize, replacement) {
                Ok(_) => None,
                Err(Some(removed)) => Some(removed),
                Err(None) => return Err(AxError::BadFileDescriptor),
            };
            description.mark_open_committed();
            let dnotify = if let Some(removed) = removed.as_ref() {
                crate::file::dnotify::detach_watch(self.id, removed.description.id())
            } else {
                None
            };
            (removed, dnotify)
        };
        drop(dnotify);
        if let Some(descriptor) = removed.as_ref() {
            self.finish_close(descriptor);
        }
        Ok(removed)
    }
}

impl Drop for FdTable {
    fn drop(&mut self) {
        if let Some(work) = self.dnotify_cleanup.get_mut().take() {
            crate::file::dnotify::publish_table_cleanup(work);
        }
    }
}

scope_local::scope_local! {
    /// The current file descriptor table.
    pub static FD_TABLE: Arc<FdTable> = scope_default_fd_table();
}

fn scope_default_fd_table() -> Arc<FdTable> {
    FD_SCOPE_DEFAULT
        .get()
        .expect("fd scope default not initialized")
        .clone()
}

/// Installs the real, empty table cloned by otherwise unpublished scopes.
/// This must run before the first scope-local item is accessed.
pub(crate) fn init_fd_scope_default() -> AxResult<()> {
    FD_SCOPE_DEFAULT
        .try_call_once(|| {
            let table = FdTable::new()?;
            Arc::try_new(table).map_err(|_| AxError::NoMemory)
        })
        .map(|_| ())
}

/// Fallibly builds a process scope around already-prepared resource pointers.
/// Scope item initialization only clones boot-prepared real Arcs; displaced
/// defaults are dropped after all fallible scope allocations have completed.
pub(crate) fn try_new_process_scope(
    fd_table: Arc<FdTable>,
    fs_context: Arc<axsync::Mutex<axfs::FsContext>>,
) -> AxResult<scope_local::Scope> {
    init_fd_scope_default()?;
    let mut scope = scope_local::Scope::try_new().map_err(|_| AxError::NoMemory)?;
    let old_fd = core::mem::replace(&mut *FD_TABLE.scope_mut(&mut scope), fd_table);
    let old_fs = core::mem::replace(&mut *axfs::FS_CONTEXT.scope_mut(&mut scope), fs_context);
    drop((old_fd, old_fs));
    Ok(scope)
}

/// Get a file-like object by `fd`.
pub fn get_file_like(fd: c_int) -> AxResult<FileHandle<dyn FileLike>> {
    let description = get_file_description(fd)?;
    Ok(FileHandle {
        file: description.inner.clone(),
        description,
    })
}

pub fn get_typed_file<T>(fd: c_int) -> AxResult<FileHandle<T>>
where
    T: FileLike + 'static,
{
    let description = get_file_description(fd)?;
    let inner = description
        .inner
        .clone()
        .downcast_arc()
        .map_err(|_| AxError::InvalidInput)?;
    Ok(FileHandle {
        description,
        file: inner,
    })
}

/// Get an open file description by `fd`.
pub fn get_file_description(fd: c_int) -> AxResult<Arc<FileDescription>> {
    FD_TABLE.get_description(fd)
}

/// Add an open file description to the file descriptor table.
pub fn add_file_description(description: Arc<FileDescription>, cloexec: bool) -> AxResult<c_int> {
    let max_nofile = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE]
        .current
        .min(AX_FILE_LIMIT as u64) as usize;
    FD_TABLE.add_at_least(description, 0, max_nofile, cloexec)
}

/// Tries to reserve the lowest available fd number. The reservation blocks
/// concurrent open/dup allocation but is intentionally absent from normal fd
/// lookup. `None` is useful to SCM_RIGHTS, where fd exhaustion truncates the
/// ancillary prefix instead of failing the already received payload.
pub(crate) fn try_reserve_fd(cloexec: bool) -> AxResult<Option<ReservedFd>> {
    let fd_table = (*FD_TABLE).clone();
    let table_id = fd_table.id();
    let limit = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE]
        .current
        .min(AX_FILE_LIMIT as u64) as usize;
    let mut reservations = fd_table.reservations.lock();
    let entries = fd_table.read();
    let fd = (0..limit).find(|&fd| entries.get(fd).is_none() && !reservations.contains(fd));
    let Some(fd) = fd else {
        return Ok(None);
    };
    reservations.insert(fd);
    drop(entries);
    drop(reservations);

    Ok(Some(ReservedFd {
        table: fd_table,
        table_id,
        fd: fd as c_int,
        cloexec,
        reserved: true,
    }))
}

/// Reserves the lowest available fd number or reports the process fd limit.
/// Open paths use this before any namespace mutation or truncate operation,
/// matching Linux's `get_unused_fd_flags()` ordering.
pub(crate) fn reserve_fd(cloexec: bool) -> AxResult<ReservedFd> {
    try_reserve_fd(cloexec)?.ok_or(AxError::TooManyOpenFiles)
}

/// Add a file to the file descriptor table.
pub fn add_file_like(f: Arc<dyn FileLike>, cloexec: bool) -> AxResult<c_int> {
    add_file_description(FileDescription::new(f)?, cloexec)
}

/// Add a file with initial file status flags to the file descriptor table.
pub fn add_file_like_with_flags(
    f: Arc<dyn FileLike>,
    cloexec: bool,
    status_flags: u32,
) -> AxResult<c_int> {
    add_file_description(FileDescription::new_with_flags(f, status_flags)?, cloexec)
}

/// Fallibly constructs an unpublished OFD with an attached subsystem
/// resource. The resource is released on allocation failure, publish failure,
/// or the final close, and is shared rather than duplicated by `dup`.
pub(crate) fn prepare_file_description_with_resource(
    f: Arc<dyn FileLike>,
    status_flags: u32,
    write_open_key: Option<ExecutableKey>,
    resource: Option<DescriptionResource>,
) -> AxResult<Arc<FileDescription>> {
    FileDescription::new_with_write_open_key_and_resource(f, status_flags, write_open_key, resource)
}

pub(crate) fn release_posix_locks_on_close(description: &FileDescription) {
    if let Ok(stat) = description.inner.stat() {
        let pid = current().as_thread().proc_data.proc.pid();
        flock::release_posix_owner_on_inode(pid, (stat.dev, stat.ino));
    }
}

/// Close a file by `fd`.
pub fn close_file_like(fd: c_int) -> AxResult {
    let f = FD_TABLE.close(fd)?;
    debug!(
        "close_file_like <= description refs: {}",
        Arc::strong_count(&f.description)
    );
    Ok(())
}

pub(crate) fn close_fd_table(table: &FdTable) -> AxResult<CloseBatch> {
    table.close_all()
}

/// Replaces the process-scope files pointer without allocating or dropping the
/// previous table under the scope lock.
pub(crate) fn replace_process_fd_table(
    scope: &mut scope_local::Scope,
    replacement: Arc<FdTable>,
) -> Arc<FdTable> {
    core::mem::replace(&mut *FD_TABLE.scope_mut(scope), replacement)
}

/// Releases process-owned record locks before dropping this process's
/// `files_struct` reference. Dropping the final reference can publish deferred
/// close notifications, so callers must not drain that work until this returns.
pub(crate) fn release_process_fd_table(pid: Pid, fd_table: Arc<FdTable>) {
    flock::release_posix_owner(pid);
    drop(fd_table);
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, boxed::Box, sync::Arc};
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::Context,
    };

    use axpoll::{IoEvents, Pollable};
    use linux_raw_sys::general::{F_WRLCK, SEEK_SET, flock64};

    use super::*;
    use crate::file::drain_deferred_description_resource_only_for_test;
    struct DropCountingFile {
        drops: Arc<AtomicUsize>,
    }

    struct LockOrderFile {
        inode: flock::InodeId,
        observer: flock::RecordLockOwner,
        locks_released_before_drop: Arc<AtomicBool>,
    }

    struct DropCountingResource(Arc<AtomicUsize>);

    impl Drop for DropCountingResource {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for DropCountingFile {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for LockOrderFile {
        fn drop(&mut self) {
            let still_locked =
                flock::mandatory_write_lock_conflicts(self.inode, self.observer, 0, 0);
            self.locks_released_before_drop
                .store(!still_locked, Ordering::SeqCst);
        }
    }

    impl Pollable for DropCountingFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileLike for DropCountingFile {
        fn stat(&self) -> AxResult<crate::file::Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("drop-counting-file"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    impl Pollable for LockOrderFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileLike for LockOrderFile {
        fn stat(&self) -> AxResult<crate::file::Kstat> {
            Err(AxError::InvalidInput)
        }

        fn path(&self) -> AxResult<Cow<'_, str>> {
            Ok(Cow::Borrowed("lock-order-file"))
        }

        fn set_nonblocking(&self, _nonblocking: bool) -> AxResult {
            Ok(())
        }
    }

    fn descriptor_for(drops: &Arc<AtomicUsize>) -> FileDescriptor {
        FileDescriptor {
            description: FileDescription::new(Arc::new(DropCountingFile {
                drops: drops.clone(),
            }))
            .unwrap(),
            cloexec: false,
        }
    }

    #[test]
    fn description_resource_is_shared_by_dup_and_released_on_final_close() {
        let file_drops = Arc::new(AtomicUsize::new(0));
        let resource_drops = Arc::new(AtomicUsize::new(0));
        let resource = Box::try_new(DropCountingResource(resource_drops.clone())).unwrap()
            as DescriptionResource;
        let description = FileDescription::new_with_write_open_key_and_resource(
            Arc::new(DropCountingFile {
                drops: file_drops.clone(),
            }),
            0,
            None,
            Some(resource),
        )
        .unwrap();
        description.mark_open_committed();

        let duplicated = description.clone();
        drop(description);
        assert_eq!(resource_drops.load(Ordering::SeqCst), 0);
        assert_eq!(file_drops.load(Ordering::SeqCst), 0);

        drop(duplicated);
        assert_eq!(file_drops.load(Ordering::SeqCst), 1);
        assert_eq!(resource_drops.load(Ordering::SeqCst), 0);
        for _ in 0..64 {
            if resource_drops.load(Ordering::SeqCst) != 0 {
                break;
            }
            // This owner deliberately has no flock, lease, write-open key, or
            // cleanup account. The host harness has no scheduler, so drain
            // only the typed resource this test is responsible for.
            drain_deferred_description_resource_only_for_test();
        }
        assert_eq!(resource_drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn close_fd_table_removes_all_descriptors_and_drops_files() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = FdTable::new().unwrap();

        assert!(table.write().add_at(0, descriptor_for(&drops)).is_ok());
        assert!(table.write().add_at(7, descriptor_for(&drops)).is_ok());
        assert_eq!(table.read().count(), 2);

        let closed = close_fd_table(&table).unwrap();

        assert_eq!(table.read().count(), 0);
        assert!(table.read().get(0).is_none());
        assert!(table.read().get(7).is_none());
        drop(closed);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn process_locks_are_released_before_fd_table_drop() {
        const EXITING_PID: Pid = 0x7fff_ff00;
        const OBSERVER_PID: Pid = EXITING_PID + 1;
        const INODE: flock::InodeId = (u64::MAX - 1, u64::MAX - 2);

        let request = flock64 {
            l_type: F_WRLCK as _,
            l_whence: SEEK_SET as _,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        flock::set_record_lock(
            INODE,
            flock::RecordLockOwner::Posix(EXITING_PID),
            0,
            0,
            &request,
            false,
        )
        .unwrap();
        assert!(flock::mandatory_write_lock_conflicts(
            INODE,
            flock::RecordLockOwner::Posix(OBSERVER_PID),
            0,
            0,
        ));

        let locks_released_before_drop = Arc::new(AtomicBool::new(false));
        let table = Arc::new(FdTable::new().unwrap());
        assert!(
            table
                .write()
                .add_at(
                    0,
                    FileDescriptor {
                        description: FileDescription::new(Arc::new(LockOrderFile {
                            inode: INODE,
                            observer: flock::RecordLockOwner::Posix(OBSERVER_PID),
                            locks_released_before_drop: locks_released_before_drop.clone(),
                        }))
                        .unwrap(),
                        cloexec: false,
                    },
                )
                .is_ok()
        );

        release_process_fd_table(EXITING_PID, table);

        assert!(locks_released_before_drop.load(Ordering::SeqCst));
    }

    #[test]
    fn fork_copy_gets_new_table_identity_but_shares_descriptions() {
        let drops = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(FdTable::new().unwrap());
        assert!(source.write().add_at(3, descriptor_for(&drops)).is_ok());

        let shared = source.clone();
        assert_eq!(source.id(), shared.id());

        let forked = FdTable::fork_copy(&source).unwrap();
        assert_ne!(source.id(), forked.id());
        let source_description = source.read().get(3).unwrap().description.clone();
        let forked_description = forked.read().get(3).unwrap().description.clone();
        assert!(Arc::ptr_eq(&source_description, &forked_description));

        drop(source_description);
        drop(forked_description);
        drop(forked);
        drop(shared);
        drop(source);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn description_revalidation_rejects_numeric_fd_reuse() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = FdTable::new().unwrap();
        let first = descriptor_for(&drops);
        let first_id = first.description.id();
        let first_description = first.description.clone();
        let second = descriptor_for(&drops);
        let second_description = second.description.clone();

        assert!(table.write().add_at(3, first).is_ok());
        let stable_handle = table.get_description(3).unwrap();
        drop(table.write().remove(3));
        assert!(table.write().add_at(3, second).is_ok());

        assert!(Arc::ptr_eq(&stable_handle, &first_description));
        assert!(!Arc::ptr_eq(&stable_handle, &second_description));
        assert_eq!(
            table.with_same_description(3, first_id, |_, _, _| Ok(())),
            Err(AxError::BadFileDescriptor)
        );
    }

    #[test]
    fn fd_reservation_is_invisible_until_publish_and_released_on_drop() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let first = descriptor_for(&drops);
        let first_description = first.description.clone();
        let second = descriptor_for(&drops);
        let second_description = second.description.clone();

        table.reservations.lock().insert(3);
        let token = ReservedFd {
            table: table.clone(),
            table_id: table.id(),
            fd: 3,
            cloexec: true,
            reserved: true,
        };
        assert!(matches!(
            table.get_description(3),
            Err(AxError::BadFileDescriptor)
        ));
        assert_eq!(
            table.add_at_least(second_description.clone(), 3, 4, false),
            Err(AxError::TooManyOpenFiles)
        );

        assert_eq!(token.publish(first_description.clone()).unwrap(), 3);
        assert!(Arc::ptr_eq(
            &table.get_description(3).unwrap(),
            &first_description
        ));
        assert!(table.get_cloexec(3).unwrap());
        drop(table.close(3).unwrap());
        table.reservations.lock().insert(3);
        let unpublished = ReservedFd {
            table: table.clone(),
            table_id: table.id(),
            fd: 3,
            cloexec: false,
            reserved: true,
        };
        drop(unpublished);

        assert_eq!(
            table.add_at_least(second_description, 3, 4, false).unwrap(),
            3
        );
    }
}
