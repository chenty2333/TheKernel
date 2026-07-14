use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ffi::c_int,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axtask::current;
use linux_raw_sys::general::RLIMIT_NOFILE;
use spin::{Mutex, Once, RwLock};
use starry_process::Pid;
pub(crate) use thekernel_linux_fd::FdTableId;
use thekernel_linux_fd::{
    CloseBatch as LinuxCloseBatch, DescriptorFlags, FdNumber, FdTable as LinuxFdTable,
    FdTableError, PreparedCloseOnExec as LinuxPreparedCloseOnExec,
    PreparedPublication as LinuxPreparedPublication, ReservationToken,
};

use super::{
    desc::{
        DescriptionResource, DescriptorPublication, FileDescription, FileDescriptor, FileHandle,
    },
    executable::ExecutableKey,
    flock,
    types::FileLike,
};
use crate::task::{AX_FILE_LIMIT, AsThread};

static NEXT_FD_TABLE_ID: AtomicU64 = AtomicU64::new(1);
static FD_SCOPE_DEFAULT: Once<Arc<FdTable>> = Once::new();

fn allocate_fd_table_id() -> AxResult<FdTableId> {
    let raw = NEXT_FD_TABLE_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
            next.checked_add(1)
        })
        .map_err(|_| AxError::TooManyOpenFiles)?;
    FdTableId::new(raw).ok_or(AxError::BadState)
}

fn map_fd_table_error(error: FdTableError) -> AxError {
    match error {
        FdTableError::BadDescriptor => AxError::BadFileDescriptor,
        FdTableError::TableFull => AxError::TooManyOpenFiles,
        FdTableError::Busy => AxError::ResourceBusy,
        FdTableError::StaleToken => AxError::BadState,
        FdTableError::GenerationExhausted => AxError::TooManyOpenFiles,
        FdTableError::InsufficientCloseStorage => AxError::BadState,
        FdTableError::NoMemory => AxError::NoMemory,
        FdTableError::Unbounded => AxError::InvalidInput,
        _ => AxError::BadState,
    }
}

fn descriptor_flags(cloexec: bool) -> DescriptorFlags {
    if cloexec {
        DescriptorFlags::CLOSE_ON_EXEC
    } else {
        DescriptorFlags::EMPTY
    }
}

fn fd_number(fd: c_int) -> AxResult<FdNumber> {
    FdNumber::from_i32(fd).ok_or(AxError::BadFileDescriptor)
}

/// Linux `files_struct` equivalent.
///
/// Its stable identity is shared by `CLONE_FILES` and replaced by ordinary
/// fork/unshare.  Keeping the entries lock inside this type also gives fd
/// lifecycle observers one place to linearize against close and replace.
pub struct FdTable {
    id: FdTableId,
    dnotify_cleanup: Mutex<Option<Box<super::dnotify::TableCleanupWork>>>,
    entries: RwLock<LinuxFdTable<Arc<FileDescription>, AX_FILE_LIMIT>>,
}

/// Linux-style fd-number reservation. The slot is unavailable to concurrent
/// open/dup operations but remains invisible to lookup until the caller has
/// finished constructing the open file description and publishes it.
pub(crate) struct ReservedFd {
    table: Arc<FdTable>,
    fd: c_int,
    reservation: Option<ReservationToken>,
}

/// Fully prepared publication of one exact descriptor into one exact table.
///
/// All fallible admission, allocation, and table validation is complete before
/// this value is returned. The final commit does not accept a caller-selected
/// table and performs only exact descriptor accounting followed by the core
/// visibility linearization.
#[must_use = "a prepared fd publication must be committed or rolled back"]
pub(crate) struct PreparedFdPublication {
    table: Arc<FdTable>,
    fd: c_int,
    description: Arc<FileDescription>,
    table_publication: Option<LinuxPreparedPublication>,
    descriptor_publication: Option<DescriptorPublication>,
}

/// Fallibly reserved storage for descriptors detached under one table lock.
/// Keeping the allocation outside the lock prevents allocator re-entry and
/// makes every subsequent push infallible for the fixed fd-table ceiling.
pub(crate) struct CloseBatch {
    table_entries: Option<LinuxCloseBatch<Arc<FileDescription>>>,
    descriptors: Vec<FileDescriptor>,
    dnotify: Vec<super::dnotify::DetachedMark>,
}

/// Full-capacity close-on-exec ownership bound to one exact files table.
///
/// Preparation reserves storage for every possible descriptor and every
/// lock-external cleanup owner. Commit therefore remains valid even if a flag
/// or descriptor changes before exec reaches its serialized table transition.
#[must_use = "prepared close-on-exec ownership should be committed or dropped"]
pub(crate) struct PreparedCloexec {
    table: Arc<FdTable>,
    table_entries: LinuxPreparedCloseOnExec<Arc<FileDescription>, AX_FILE_LIMIT>,
    descriptors: Vec<FileDescriptor>,
    dnotify: Vec<super::dnotify::DetachedMark>,
}

impl CloseBatch {
    fn with_capacity(capacity: usize) -> AxResult<Self> {
        let table_entries =
            LinuxCloseBatch::try_with_capacity(capacity).map_err(map_fd_table_error)?;
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        let mut dnotify = Vec::new();
        dnotify
            .try_reserve_exact(capacity)
            .map_err(|_| AxError::NoMemory)?;
        Ok(Self {
            table_entries: Some(table_entries),
            descriptors,
            dnotify,
        })
    }

    fn finish_dnotify(&mut self) {
        let detached = core::mem::take(&mut self.dnotify);
        drop(detached);
    }

    fn table_entries_mut(&mut self) -> AxResult<&mut LinuxCloseBatch<Arc<FileDescription>>> {
        self.table_entries.as_mut().ok_or(AxError::BadState)
    }

    fn finish_table_entries(&mut self) -> AxResult<()> {
        let entries = self.table_entries.take().ok_or(AxError::BadState)?;
        for entry in entries.into_entries() {
            let (description, _) = entry.into_parts();
            self.descriptors.push(FileDescriptor { description });
        }
        Ok(())
    }
}

impl ReservedFd {
    pub(crate) const fn fd(&self) -> c_int {
        self.fd
    }

    /// Completes every fallible admission needed to publish this exact number.
    pub(crate) fn prepare_publication(
        mut self,
        description: Arc<FileDescription>,
    ) -> AxResult<PreparedFdPublication> {
        let descriptor_publication = description.begin_descriptor_publication()?;
        let reservation = self.reservation.take().ok_or(AxError::BadState)?;
        let table_publication = match self
            .table
            .entries
            .write()
            .prepare_publication(reservation, description.clone())
        {
            Ok(publication) => publication,
            Err(error) => {
                self.reservation = Some(error.reservation);
                drop(descriptor_publication);
                return Err(map_fd_table_error(error.error));
            }
        };
        Ok(PreparedFdPublication {
            table: Arc::clone(&self.table),
            fd: self.fd,
            description,
            table_publication: Some(table_publication),
            descriptor_publication: Some(descriptor_publication),
        })
    }

    /// Publishes the fully constructed description into this reserved number.
    /// Compatibility callers still receive `AxResult`, but every failure now
    /// occurs in preparation and the final visibility transition is infallible.
    pub(crate) fn publish(self, description: Arc<FileDescription>) -> AxResult<c_int> {
        Ok(self.prepare_publication(description)?.commit())
    }
}

impl PreparedFdPublication {
    pub(crate) const fn fd(&self) -> c_int {
        self.fd
    }

    /// Makes the exact prepared slot visible. No allocation, table selection,
    /// validation, or fallible operation remains at this point.
    pub(crate) fn commit(mut self) -> c_int {
        if let Some(publication) = self.descriptor_publication.take() {
            publication.commit();
        }
        self.description.mark_open_committed();
        if let Some(publication) = self.table_publication.take() {
            // The exact table lock makes the Pending -> Visible linearization
            // mutually exclusive with fork snapshots, close-on-exec scans,
            // and every other whole-table operation.
            let entries = self.table.entries.write();
            let _token = publication.commit();
            drop(entries);
        }
        self.fd
    }
}

impl Drop for PreparedFdPublication {
    fn drop(&mut self) {
        let Some(publication) = self.table_publication.take() else {
            return;
        };
        let detached = self.table.entries.write().cancel_prepared(publication);
        match detached {
            Ok(entry) => drop(entry),
            Err(error) => {
                error!(
                    "prepared fd rollback lost exact table ownership: {:?}",
                    error.error
                );
                drop(error.publication);
            }
        }
    }
}

impl Drop for ReservedFd {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take()
            && let Err(error) = self.table.entries.write().cancel_reservation(reservation)
        {
            error!("fd reservation rollback failed: {error:?}");
        }
    }
}

impl FdTable {
    pub fn new() -> AxResult<Self> {
        let id = allocate_fd_table_id()?;
        Ok(Self {
            id,
            dnotify_cleanup: Mutex::new(None),
            entries: RwLock::new(LinuxFdTable::try_new(id).map_err(map_fd_table_error)?),
        })
    }

    pub fn fork_copy(&self) -> AxResult<Self> {
        let id = allocate_fd_table_id()?;
        let source = self.entries.read();
        let entries = source.fork_copy(id).map_err(map_fd_table_error)?;
        let mut committed = 0;
        let mut publication_error = None;
        for (_, entry) in entries.iter() {
            let publication = match entry.description().begin_descriptor_publication() {
                Ok(publication) => publication,
                Err(error) => {
                    publication_error = Some(error);
                    break;
                }
            };
            publication.commit();
            committed += 1;
        }
        if let Some(error) = publication_error {
            for (_, committed_entry) in entries.iter().take(committed) {
                committed_entry.description().descriptor_closed();
            }
            drop(entries);
            drop(source);
            return Err(error);
        }
        drop(source);
        Ok(Self {
            id,
            dnotify_cleanup: Mutex::new(None),
            entries: RwLock::new(entries),
        })
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
        let fd_number = fd_number(fd)?;
        let id = self.id();
        let entries = self.entries.read();
        let description = entries
            .get(fd_number)
            .map_err(map_fd_table_error)?
            .description();
        if description.id() != expected {
            return Err(AxError::BadFileDescriptor);
        }
        f(id, fd, description)
    }

    pub(crate) fn add_at_least(
        &self,
        description: Arc<FileDescription>,
        min_fd: usize,
        limit: usize,
        cloexec: bool,
    ) -> AxResult<c_int> {
        let publication = description.begin_descriptor_publication()?;
        let mut entries = self.entries.write();
        let reservation = entries
            .reserve(min_fd, limit, descriptor_flags(cloexec))
            .map_err(map_fd_table_error)?;
        let fd = reservation.fd();
        let published = entries.publish(reservation, description.clone());
        if published.is_ok() {
            publication.commit();
            description.mark_open_committed();
        } else {
            drop(entries);
            drop(publication);
            return match published {
                Ok(_) => Err(AxError::BadState),
                Err(error) => Err(map_fd_table_error(error.error)),
            };
        }
        drop(entries);
        match published {
            Ok(_) => Ok(fd.get() as c_int),
            Err(error) => Err(map_fd_table_error(error.error)),
        }
    }

    pub(crate) fn get_description(&self, fd: c_int) -> AxResult<Arc<FileDescription>> {
        let fd = fd_number(fd)?;
        self.entries
            .read()
            .get(fd)
            .map(|entry| Arc::clone(entry.description()))
            .map_err(map_fd_table_error)
    }

    pub(crate) fn get_description_number(&self, fd: u32) -> AxResult<Arc<FileDescription>> {
        if fd as usize >= AX_FILE_LIMIT {
            return Err(AxError::BadFileDescriptor);
        }
        self.entries
            .read()
            .get(FdNumber::new(fd))
            .map(|entry| Arc::clone(entry.description()))
            .map_err(map_fd_table_error)
    }

    /// Produces an ordered, allocation-admitted snapshot for procfs without
    /// exposing the table lock or a second descriptor registry.
    pub(crate) fn try_fd_numbers(&self) -> AxResult<Vec<u32>> {
        loop {
            let required = self.entries.read().len();
            let mut numbers = Vec::new();
            numbers
                .try_reserve_exact(required)
                .map_err(|_| AxError::NoMemory)?;
            let entries = self.entries.read();
            if entries.len() > numbers.capacity() {
                drop(entries);
                continue;
            }
            numbers.extend(entries.iter().map(|(fd, _)| fd.get()));
            return Ok(numbers);
        }
    }

    pub(crate) fn get_cloexec(&self, fd: c_int) -> AxResult<bool> {
        let fd = fd_number(fd)?;
        self.entries
            .read()
            .get(fd)
            .map(|entry| entry.flags().contains(DescriptorFlags::CLOSE_ON_EXEC))
            .map_err(map_fd_table_error)
    }

    pub(crate) fn set_cloexec(&self, fd: c_int, cloexec: bool) -> AxResult<()> {
        let fd = fd_number(fd)?;
        self.entries
            .write()
            .set_close_on_exec(fd, cloexec)
            .map_err(map_fd_table_error)
    }

    pub(crate) fn mark_cloexec_range(&self, first: u32, last: u32) {
        self.entries
            .write()
            .mark_close_on_exec_range(FdNumber::new(first), FdNumber::new(last));
    }

    fn finish_close(&self, removed: &FileDescriptor) {
        release_posix_locks_on_close(&removed.description);
        removed.description.descriptor_closed();
    }

    pub(crate) fn close(&self, fd: c_int) -> AxResult<FileDescriptor> {
        let fd = fd_number(fd)?;
        let (entry, dnotify) = {
            let mut entries = self.entries.write();
            let entry = entries.close(fd).map_err(map_fd_table_error)?;
            let dnotify = crate::file::dnotify::detach_watch(self.id, entry.description().id());
            (entry, dnotify)
        };
        drop(dnotify);
        let (description, _) = entry.into_parts();
        let removed = FileDescriptor { description };
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
        let fd = FdNumber::from_i32(fd)?;
        let (entry, dnotify) = {
            let mut entries = self.entries.write();
            if !matches!(
                entries.get(fd),
                Ok(entry) if entry.description().id() == expected
            ) {
                return None;
            }
            let entry = entries.close(fd).ok()?;
            let dnotify = crate::file::dnotify::detach_watch(self.id, entry.description().id());
            (entry, dnotify)
        };
        drop(dnotify);
        let (description, _) = entry.into_parts();
        let removed = FileDescriptor { description };
        self.finish_close(&removed);
        Some(removed)
    }

    pub(crate) fn close_range(&self, first: u32, last: u32) -> AxResult<CloseBatch> {
        let mut removed = CloseBatch::with_capacity(AX_FILE_LIMIT)?;
        {
            let mut entries = self.entries.write();
            entries
                .close_range(
                    FdNumber::new(first),
                    FdNumber::new(last),
                    removed.table_entries_mut()?,
                )
                .map_err(map_fd_table_error)?;
            for entry in removed
                .table_entries
                .as_ref()
                .ok_or(AxError::BadState)?
                .entries()
            {
                if let Some(mark) =
                    crate::file::dnotify::detach_watch(self.id, entry.description().id())
                {
                    removed.dnotify.push(mark);
                }
            }
        }
        removed.finish_dnotify();
        removed.finish_table_entries()?;
        for descriptor in &removed.descriptors {
            self.finish_close(descriptor);
        }
        Ok(removed)
    }

    /// Prepares a table-bound, full-capacity close-on-exec transaction.
    pub(crate) fn prepare_cloexec(self: &Arc<Self>) -> AxResult<PreparedCloexec> {
        let table_entries = self
            .entries
            .read()
            .prepare_close_on_exec()
            .map_err(map_fd_table_error)?;
        let mut descriptors = Vec::new();
        descriptors
            .try_reserve_exact(AX_FILE_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        let mut dnotify = Vec::new();
        dnotify
            .try_reserve_exact(AX_FILE_LIMIT)
            .map_err(|_| AxError::NoMemory)?;
        Ok(PreparedCloexec {
            table: Arc::clone(self),
            table_entries,
            descriptors,
            dnotify,
        })
    }

    pub(crate) fn close_all(&self) -> AxResult<CloseBatch> {
        let mut removed = CloseBatch::with_capacity(AX_FILE_LIMIT)?;
        {
            let mut entries = self.entries.write();
            entries
                .close_all(removed.table_entries_mut()?)
                .map_err(map_fd_table_error)?;
            for entry in removed
                .table_entries
                .as_ref()
                .ok_or(AxError::BadState)?
                .entries()
            {
                if let Some(mark) =
                    crate::file::dnotify::detach_watch(self.id, entry.description().id())
                {
                    removed.dnotify.push(mark);
                }
            }
        }
        removed.finish_dnotify();
        removed.finish_table_entries()?;
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
        let old_fd = fd_number(old_fd)?;
        let new_fd = fd_number(new_fd)?;
        if new_fd.index() >= AX_FILE_LIMIT {
            return Err(AxError::BadFileDescriptor);
        }
        if old_fd == new_fd {
            self.entries
                .read()
                .get(old_fd)
                .map_err(map_fd_table_error)?;
            return Ok(None);
        }
        let (entry, dnotify) = {
            let mut entries = self.entries.write();
            let source = Arc::clone(
                entries
                    .get(old_fd)
                    .map_err(map_fd_table_error)?
                    .description(),
            );
            let publication = source.begin_descriptor_publication()?;
            let duplicated = entries.duplicate_replace(old_fd, new_fd, descriptor_flags(cloexec));
            let (_, removed) = match duplicated {
                Ok(result) => result,
                Err(error) => {
                    drop(entries);
                    drop(publication);
                    return Err(map_fd_table_error(error));
                }
            };
            publication.commit();
            let dnotify = if let Some(removed) = removed.as_ref() {
                crate::file::dnotify::detach_watch(self.id, removed.description().id())
            } else {
                None
            };
            (removed, dnotify)
        };
        drop(dnotify);
        let removed = entry.map(|entry| {
            let (description, _) = entry.into_parts();
            FileDescriptor { description }
        });
        if let Some(descriptor) = removed.as_ref() {
            self.finish_close(descriptor);
        }
        Ok(removed)
    }
}

impl PreparedCloexec {
    /// Detaches and finishes all descriptors currently marked close-on-exec.
    /// The exact table lock serializes this commit with descriptor publication,
    /// fork snapshots, flag changes, and other whole-table operations.
    pub(crate) fn commit(mut self) {
        let committed = {
            let mut entries = self.table.entries.write();
            let committed = entries.commit_close_on_exec(self.table_entries);
            for entry in committed.entries() {
                if let Some(mark) =
                    crate::file::dnotify::detach_watch(self.table.id, entry.description().id())
                {
                    self.dnotify.push(mark);
                }
            }
            committed
        };
        drop(self.dnotify);
        for entry in committed.into_entries() {
            let (description, _) = entry.into_parts();
            self.descriptors.push(FileDescriptor { description });
        }
        for descriptor in &self.descriptors {
            self.table.finish_close(descriptor);
        }
    }
}

impl Drop for FdTable {
    fn drop(&mut self) {
        let entries = self.entries.get_mut();
        for fd in 0..AX_FILE_LIMIT {
            if let Ok(entry) = entries.close(FdNumber::new(fd as u32)) {
                entry.description().descriptor_closed();
                drop(entry);
            }
        }
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
    let limit = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE]
        .current
        .min(AX_FILE_LIMIT as u64) as usize;
    let reservation = match fd_table
        .entries
        .write()
        .reserve(0, limit, descriptor_flags(cloexec))
    {
        Ok(reservation) => reservation,
        Err(FdTableError::TableFull) => return Ok(None),
        Err(error) => return Err(map_fd_table_error(error)),
    };
    let fd = reservation.fd().get() as c_int;

    Ok(Some(ReservedFd {
        table: fd_table,
        fd,
        reservation: Some(reservation),
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

/// Constructs an unpublished OFD which owns the exact lease-open admission
/// acquired before filesystem open. Publication converts that pending record
/// to visible state; final OFD drop queues its task-context release.
pub(crate) fn prepare_file_description_with_open_lease(
    f: Arc<dyn FileLike>,
    status_flags: u32,
    write_open_key: Option<ExecutableKey>,
    resource: Option<DescriptionResource>,
    open_lease_admission: super::lease::OpenLeaseAdmission,
    vfs_open_credential: Arc<crate::task::Cred>,
) -> AxResult<Arc<FileDescription>> {
    FileDescription::new_with_open_lease_admission_and_resource(
        f,
        status_flags,
        write_open_key,
        resource,
        open_lease_admission,
        vfs_open_credential,
    )
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
fn release_process_fd_table_with(
    pid: Pid,
    fd_table: Arc<FdTable>,
    release_locks: impl FnOnce(Pid),
) {
    release_locks(pid);
    drop(fd_table);
}

pub(crate) fn release_process_fd_table(pid: Pid, fd_table: Arc<FdTable>) {
    release_process_fd_table_with(pid, fd_table, flock::release_posix_owner);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{borrow::Cow, boxed::Box, sync::Arc, task::Wake};
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use axpoll::{IoEvents, Pollable};

    use super::*;
    use crate::file::{
        desc::DescriptorCloseRegistrationError, drain_deferred_description_resource_only_for_test,
    };
    struct DropCountingFile {
        drops: Arc<AtomicUsize>,
    }

    struct LockOrderFile {
        locks_released: Arc<AtomicBool>,
        locks_released_before_drop: Arc<AtomicBool>,
    }

    struct DropCountingResource(Arc<AtomicUsize>);

    struct CountingWake(AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

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
            self.locks_released_before_drop
                .store(self.locks_released.load(Ordering::SeqCst), Ordering::SeqCst);
        }
    }

    impl Pollable for DropCountingFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
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

        fn register<'a>(
            &'a self,
            _context: &mut Context<'_>,
            _events: IoEvents,
        ) -> Result<axpoll::PollRegistration<'a>, axpoll::PollRegistrationError> {
            axpoll::PollRegistration::empty()
        }
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

        table
            .add_at_least(descriptor_for(&drops).description, 0, 1, false)
            .unwrap();
        table
            .add_at_least(descriptor_for(&drops).description, 7, 8, false)
            .unwrap();
        assert_eq!(table.entries.read().len(), 2);

        let closed = close_fd_table(&table).unwrap();

        assert_eq!(table.entries.read().len(), 0);
        assert!(table.entries.read().get(FdNumber::new(0)).is_err());
        assert!(table.entries.read().get(FdNumber::new(7)).is_err());
        drop(closed);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn process_locks_are_released_before_fd_table_drop() {
        const EXITING_PID: Pid = 0x7fff_ff00;

        let locks_released = Arc::new(AtomicBool::new(false));
        let locks_released_before_drop = Arc::new(AtomicBool::new(false));
        let table = Arc::new(FdTable::new().unwrap());
        let description = FileDescription::new(Arc::new(LockOrderFile {
            locks_released: locks_released.clone(),
            locks_released_before_drop: locks_released_before_drop.clone(),
        }))
        .unwrap();
        table.add_at_least(description, 0, 1, false).unwrap();

        release_process_fd_table_with(EXITING_PID, table, |pid| {
            assert_eq!(pid, EXITING_PID);
            locks_released.store(true, Ordering::SeqCst);
        });

        assert!(locks_released_before_drop.load(Ordering::SeqCst));
    }

    #[test]
    fn fork_copy_gets_new_table_identity_but_shares_descriptions() {
        let drops = Arc::new(AtomicUsize::new(0));
        let source = Arc::new(FdTable::new().unwrap());
        source
            .add_at_least(descriptor_for(&drops).description, 3, 4, false)
            .unwrap();

        let shared = source.clone();
        assert_eq!(source.id(), shared.id());

        let forked = FdTable::fork_copy(&source).unwrap();
        assert_ne!(source.id(), forked.id());
        let source_description = source.get_description(3).unwrap();
        let forked_description = forked.get_description(3).unwrap();
        assert!(Arc::ptr_eq(&source_description, &forked_description));

        drop(source_description);
        drop(forked_description);
        drop(forked);
        drop(shared);
        drop(source);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn last_descriptor_close_is_counted_across_dup_and_fork() {
        let drops = Arc::new(AtomicUsize::new(0));
        let description = descriptor_for(&drops).description;
        let source = FdTable::new().unwrap();
        source
            .add_at_least(description.clone(), 3, 4, false)
            .unwrap();
        source
            .add_at_least(description.clone(), 7, 8, false)
            .unwrap();
        let forked = source.fork_copy().unwrap();
        assert_eq!(description.descriptor_reference_count(), 4);

        let wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let waker = Waker::from(wake.clone());
        let _registration = description.register_descriptor_close(&waker).unwrap();

        drop(source.close(3).unwrap());
        drop(source.close(7).unwrap());
        assert_eq!(description.descriptor_reference_count(), 2);
        assert_eq!(wake.0.load(Ordering::SeqCst), 0);

        drop(forked);
        assert_eq!(description.descriptor_reference_count(), 0);
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            description.register_descriptor_close(&waker).err(),
            Some(DescriptorCloseRegistrationError::Closed)
        );
    }

    #[test]
    fn republished_ofd_uses_a_fresh_descriptor_close_epoch() {
        let drops = Arc::new(AtomicUsize::new(0));
        let description = descriptor_for(&drops).description;
        let table = FdTable::new().unwrap();
        table
            .add_at_least(description.clone(), 3, 4, false)
            .unwrap();

        let first_wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let first_waker = Waker::from(first_wake.clone());
        let _first_registration = description.register_descriptor_close(&first_waker).unwrap();
        drop(table.close(3).unwrap());
        assert_eq!(first_wake.0.load(Ordering::SeqCst), 1);

        // Models a retained SCM_RIGHTS OFD being installed after the sender
        // closed its final descriptor. The old close source stays terminal;
        // no stale epoll generation is revived in the new descriptor epoch.
        table
            .add_at_least(description.clone(), 3, 4, false)
            .unwrap();
        let second_wake = Arc::new(CountingWake(AtomicUsize::new(0)));
        let second_waker = Waker::from(second_wake.clone());
        let _second_registration = description
            .register_descriptor_close(&second_waker)
            .unwrap();
        drop(table.close(3).unwrap());

        assert_eq!(first_wake.0.load(Ordering::SeqCst), 1);
        assert_eq!(second_wake.0.load(Ordering::SeqCst), 1);
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

        table.add_at_least(first.description, 3, 4, false).unwrap();
        let stable_handle = table.get_description(3).unwrap();
        drop(table.close(3).unwrap());
        table.add_at_least(second.description, 3, 4, false).unwrap();

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

        let reservation = table
            .entries
            .write()
            .reserve(3, 4, DescriptorFlags::CLOSE_ON_EXEC)
            .unwrap();
        let token = ReservedFd {
            table: table.clone(),
            fd: 3,
            reservation: Some(reservation),
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
        let reservation = table
            .entries
            .write()
            .reserve(3, 4, DescriptorFlags::EMPTY)
            .unwrap();
        let unpublished = ReservedFd {
            table: table.clone(),
            fd: 3,
            reservation: Some(reservation),
        };
        drop(unpublished);

        assert_eq!(
            table.add_at_least(second_description, 3, 4, false).unwrap(),
            3
        );
    }

    fn reserve_test_fd(table: &Arc<FdTable>, cloexec: bool) -> ReservedFd {
        let reservation = table
            .entries
            .write()
            .reserve(3, 4, descriptor_flags(cloexec))
            .unwrap();
        ReservedFd {
            table: Arc::clone(table),
            fd: 3,
            reservation: Some(reservation),
        }
    }

    #[test]
    fn prepared_fd_drop_refunds_table_and_descriptor_admission() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();

        assert_eq!(prepared.fd(), 3);
        assert_eq!(description.descriptor_reference_count(), 0);
        assert_eq!(description.descriptor_pending_publication_count(), 1);
        assert!(matches!(
            table.get_description(3),
            Err(AxError::BadFileDescriptor)
        ));
        drop(prepared);

        assert_eq!(description.descriptor_reference_count(), 0);
        assert_eq!(description.descriptor_pending_publication_count(), 0);
        let replacement = descriptor_for(&drops).description;
        assert_eq!(
            reserve_test_fd(&table, false).publish(replacement).unwrap(),
            3
        );
    }

    #[test]
    fn prepared_fd_commit_publishes_exact_description_and_cloexec_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();

        assert_eq!(prepared.commit(), 3);
        assert!(Arc::ptr_eq(
            &table.get_description(3).unwrap(),
            &description
        ));
        assert!(table.get_cloexec(3).unwrap());
        assert_eq!(description.descriptor_reference_count(), 1);
        assert_eq!(description.descriptor_pending_publication_count(), 0);

        drop(table.close(3).unwrap());
        assert_eq!(description.descriptor_reference_count(), 0);
    }

    #[test]
    fn prepared_fd_rollback_is_bound_to_its_exact_table() {
        let drops = Arc::new(AtomicUsize::new(0));
        let one = Arc::new(FdTable::new().unwrap());
        let two = Arc::new(FdTable::new().unwrap());
        let first = descriptor_for(&drops).description;
        let second = descriptor_for(&drops).description;
        let prepared_one = reserve_test_fd(&one, false)
            .prepare_publication(first.clone())
            .unwrap();
        let prepared_two = reserve_test_fd(&two, false)
            .prepare_publication(second.clone())
            .unwrap();

        drop(prepared_one);
        assert_eq!(prepared_two.commit(), 3);
        assert!(matches!(
            one.get_description(3),
            Err(AxError::BadFileDescriptor)
        ));
        assert!(Arc::ptr_eq(&two.get_description(3).unwrap(), &second));
        assert_eq!(first.descriptor_pending_publication_count(), 0);
        assert_eq!(second.descriptor_reference_count(), 1);
    }

    #[test]
    fn prepared_fd_keeps_slot_busy_until_exact_visibility_commit() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();
        let observer = Arc::clone(&table);

        let blocked = std::thread::spawn(move || {
            observer
                .entries
                .write()
                .reserve(3, 4, DescriptorFlags::EMPTY)
        })
        .join()
        .unwrap();
        assert_eq!(blocked, Err(FdTableError::TableFull));
        assert_eq!(prepared.commit(), 3);

        let observed = std::thread::spawn(move || table.get_description(3).unwrap())
            .join()
            .unwrap();
        assert!(Arc::ptr_eq(&observed, &description));
    }

    #[test]
    fn prepared_fd_visibility_orders_open_and_descriptor_commits() {
        use std::sync::Barrier;

        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();
        let start = Arc::new(Barrier::new(2));
        let observer_table = Arc::clone(&table);
        let observer_start = Arc::clone(&start);
        let observer = std::thread::spawn(move || {
            observer_start.wait();
            loop {
                match observer_table.get_description(3) {
                    Ok(observed) => {
                        return (
                            observed.open_committed_for_test(),
                            observed.descriptor_reference_count(),
                        );
                    }
                    Err(AxError::BadFileDescriptor) => std::thread::yield_now(),
                    Err(_) => return (false, 0),
                }
            }
        });

        start.wait();
        assert_eq!(prepared.commit(), 3);
        assert_eq!(observer.join().unwrap(), (true, 1));
    }

    #[test]
    fn prepared_fd_commit_linearizes_after_an_inflight_cloexec_scan() {
        use std::{sync::mpsc, time::Duration};

        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();
        let mut entries = table.entries.write();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let committer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let fd = prepared.commit();
            finished_tx.send(fd).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let mut removed = LinuxCloseBatch::try_with_capacity(1).unwrap();
        entries.close_on_exec(&mut removed).unwrap();
        assert!(removed.entries().is_empty());
        drop(entries);

        assert_eq!(finished_rx.recv().unwrap(), 3);
        committer.join().unwrap();
        assert!(Arc::ptr_eq(
            &table.get_description(3).unwrap(),
            &description
        ));
    }

    #[test]
    fn prepared_fd_commit_linearizes_after_an_inflight_fork_snapshot() {
        use std::{sync::mpsc, time::Duration};

        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let description = descriptor_for(&drops).description;
        let prepared = reserve_test_fd(&table, true)
            .prepare_publication(description.clone())
            .unwrap();
        let entries = table.entries.read();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let committer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let fd = prepared.commit();
            finished_tx.send(fd).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            finished_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        let fork = entries.fork_copy(allocate_fd_table_id().unwrap()).unwrap();
        assert!(matches!(
            fork.get(FdNumber::new(3)),
            Err(FdTableError::BadDescriptor)
        ));
        drop(entries);

        assert_eq!(finished_rx.recv().unwrap(), 3);
        committer.join().unwrap();
        assert!(Arc::ptr_eq(
            &table.get_description(3).unwrap(),
            &description
        ));
    }

    #[test]
    fn prepared_cloexec_full_capacity_covers_later_flags_and_descriptors() {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(FdTable::new().unwrap());
        let first = descriptor_for(&drops).description;
        let second = descriptor_for(&drops).description;
        table.add_at_least(first.clone(), 3, 4, false).unwrap();
        let prepared = table.prepare_cloexec().unwrap();

        table.set_cloexec(3, true).unwrap();
        table.add_at_least(second.clone(), 4, 5, true).unwrap();
        prepared.commit();

        assert!(matches!(
            table.get_description(3),
            Err(AxError::BadFileDescriptor)
        ));
        assert!(matches!(
            table.get_description(4),
            Err(AxError::BadFileDescriptor)
        ));
        assert_eq!(first.descriptor_reference_count(), 0);
        assert_eq!(second.descriptor_reference_count(), 0);
    }
}
