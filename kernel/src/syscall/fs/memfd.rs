use alloc::string::String;
use core::{
    ffi::c_char,
    fmt::Write as _,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::NodePermission;
use linux_raw_sys::general::{AT_FDCWD, MFD_CLOEXEC, O_CLOEXEC, O_CREAT, O_EXCL, O_RDWR};

use super::fd_ops::openat_inner;
use crate::{
    file::{FD_TABLE, File, get_file_description, memfd},
    mm::UserConstPtr,
};

const MEMFD_NAME_MAX: usize = 249;
const MEMFD_DIR: &str = "/tmp/memfd";

static MEMFD_COUNTER: AtomicU64 = AtomicU64::new(0);

fn memfd_path(id: u64) -> AxResult<String> {
    let capacity = MEMFD_DIR
        .len()
        .checked_add(1 + ".memfd-".len() + 16)
        .ok_or(AxError::NoMemory)?;
    let mut path = String::new();
    path.try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    write!(&mut path, "{MEMFD_DIR}/.memfd-{id:016x}").map_err(|_| AxError::Io)?;
    Ok(path)
}

fn validate_memfd_name(name: UserConstPtr<c_char>) -> AxResult<()> {
    let start = name.address().as_usize();
    for offset in 0..=MEMFD_NAME_MAX {
        let byte = *UserConstPtr::<u8>::from(start + offset).get_as_ref()?;
        if byte == 0 {
            return Ok(());
        }
    }
    Err(AxError::InvalidInput)
}

fn ensure_memfd_dir() -> AxResult<()> {
    let fs = FS_CONTEXT.lock();
    match fs.resolve(MEMFD_DIR) {
        Ok(loc) if loc.is_dir() => Ok(()),
        Ok(_) => Err(AxError::NotADirectory),
        Err(AxError::NotFound) => {
            fs.create_dir(MEMFD_DIR, NodePermission::from_bits_truncate(0o777))?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

pub fn sys_memfd_create(name: UserConstPtr<c_char>, flags: u32) -> AxResult<isize> {
    validate_memfd_name(name)?;
    if flags & !memfd::MEMFD_SUPPORTED_CREATE_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    ensure_memfd_dir()?;
    let allow_sealing = flags & linux_raw_sys::general::MFD_ALLOW_SEALING != 0;
    let mut open_flags = O_RDWR | O_CREAT | O_EXCL;
    if flags & MFD_CLOEXEC != 0 {
        open_flags |= O_CLOEXEC;
    }

    for _ in 0..64 {
        let id = MEMFD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = memfd_path(id)?;
        match openat_inner(AT_FDCWD as _, &path, open_flags as i32, 0o600) {
            Ok(fd) => {
                let description = get_file_description(fd as i32)?;
                let expected = description.id();
                let install = description
                    .inner
                    .downcast_ref::<File>()
                    .ok_or(AxError::BadFileDescriptor)
                    .and_then(|file| {
                        memfd::install_memfd_state(file.inner().location(), allow_sealing).map(drop)
                    });
                if let Err(error) = install {
                    drop(description);
                    let removed = FD_TABLE.close_if_same(fd as i32, expected);
                    drop(removed);
                    crate::file::inotify::wait_current_close_notifications();
                    return Err(error);
                }
                return Ok(fd);
            }
            Err(AxError::AlreadyExists) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(AxError::AlreadyExists)
}
