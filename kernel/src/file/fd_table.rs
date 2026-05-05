use alloc::sync::Arc;
use core::ffi::c_int;

use axerrno::{AxError, AxResult};
use axtask::current;
use flatten_objects::FlattenObjects;
use linux_raw_sys::general::RLIMIT_NOFILE;
use spin::RwLock;

use super::{
    desc::{FileDescription, FileDescriptor, FileHandle},
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

/// Close a file by `fd`.
pub fn close_file_like(fd: c_int) -> AxResult {
    let f = FD_TABLE
        .write()
        .remove(fd as usize)
        .ok_or(AxError::BadFileDescriptor)?;
    debug!(
        "close_file_like <= description refs: {}",
        Arc::strong_count(&f.description)
    );
    Ok(())
}
