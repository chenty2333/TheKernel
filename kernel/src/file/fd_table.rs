use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_int;

use axerrno::{AxError, AxResult};
use axtask::current;
use flatten_objects::FlattenObjects;
use linux_raw_sys::general::RLIMIT_NOFILE;
use spin::RwLock;

use super::{
    desc::{FileDescription, FileDescriptor, FileHandle},
    executable::ExecutableKey,
    flock,
    types::FileLike,
};
use crate::task::{AX_FILE_LIMIT, AsThread};

scope_local::scope_local! {
    /// The current file descriptor table.
    pub static FD_TABLE: Arc<RwLock<FlattenObjects<FileDescriptor, AX_FILE_LIMIT>>> = Arc::default();
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
    FD_TABLE
        .read()
        .get(fd as usize)
        .map(|fd| fd.description.clone())
        .ok_or(AxError::BadFileDescriptor)
}

/// Add an open file description to the file descriptor table.
pub fn add_file_description(description: Arc<FileDescription>, cloexec: bool) -> AxResult<c_int> {
    let max_nofile = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE].current;
    let mut table = FD_TABLE.write();
    if table.count() as u64 >= max_nofile {
        return Err(AxError::TooManyOpenFiles);
    }
    let fd = FileDescriptor {
        description,
        cloexec,
    };
    Ok(table.add(fd).map_err(|_| AxError::TooManyOpenFiles)? as c_int)
}

/// Add a file to the file descriptor table.
pub fn add_file_like(f: Arc<dyn FileLike>, cloexec: bool) -> AxResult<c_int> {
    add_file_description(FileDescription::new(f), cloexec)
}

/// Add a file with initial file status flags to the file descriptor table.
pub fn add_file_like_with_flags(
    f: Arc<dyn FileLike>,
    cloexec: bool,
    status_flags: u32,
) -> AxResult<c_int> {
    add_file_description(FileDescription::new_with_flags(f, status_flags), cloexec)
}

pub(crate) fn add_file_like_with_flags_and_write_open_key(
    f: Arc<dyn FileLike>,
    cloexec: bool,
    status_flags: u32,
    write_open_key: Option<ExecutableKey>,
) -> AxResult<c_int> {
    add_file_description(
        FileDescription::new_with_write_open_key(f, status_flags, write_open_key),
        cloexec,
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
    let f = FD_TABLE
        .write()
        .remove(fd as usize)
        .ok_or(AxError::BadFileDescriptor)?;
    release_posix_locks_on_close(&f.description);
    debug!(
        "close_file_like <= description refs: {}",
        Arc::strong_count(&f.description)
    );
    Ok(())
}

pub(crate) fn close_fd_table(
    table: &mut FlattenObjects<FileDescriptor, AX_FILE_LIMIT>,
) -> Vec<FileDescriptor> {
    let mut closed = Vec::new();
    let fds = table.ids().collect::<Vec<_>>();
    for fd in fds {
        if let Some(f) = table.remove(fd) {
            release_posix_locks_on_close(&f.description);
            closed.push(f);
        }
    }
    closed
}

pub(crate) fn close_process_fd_table(scope: &mut scope_local::Scope) -> Vec<FileDescriptor> {
    let mut fd_table = FD_TABLE.scope_mut(scope);
    let mut closed = Vec::new();
    if Arc::strong_count(&*fd_table) == 1 {
        closed = close_fd_table(&mut fd_table.write());
    }
    *fd_table = Arc::default();
    closed
}

#[cfg(test)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc};
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::Context,
    };

    use axpoll::{IoEvents, Pollable};

    use super::*;
    use crate::task::AX_FILE_LIMIT;

    struct DropCountingFile {
        drops: Arc<AtomicUsize>,
    }

    impl Drop for DropCountingFile {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
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

        fn path(&self) -> Cow<'_, str> {
            Cow::Borrowed("drop-counting-file")
        }
    }

    fn descriptor_for(drops: &Arc<AtomicUsize>) -> FileDescriptor {
        FileDescriptor {
            description: FileDescription::new(Arc::new(DropCountingFile {
                drops: drops.clone(),
            })),
            cloexec: false,
        }
    }

    #[test]
    fn close_fd_table_removes_all_descriptors_and_drops_files() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut table = FlattenObjects::<FileDescriptor, AX_FILE_LIMIT>::new();

        assert!(table.add_at(0, descriptor_for(&drops)).is_ok());
        assert!(table.add_at(7, descriptor_for(&drops)).is_ok());
        assert_eq!(table.count(), 2);

        let closed = close_fd_table(&mut table);

        assert_eq!(table.count(), 0);
        assert!(table.get(0).is_none());
        assert!(table.get(7).is_none());
        drop(closed);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}
