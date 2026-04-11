use alloc::format;
use core::{
    ffi::c_char,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs::FS_CONTEXT;
use axfs_ng_vfs::NodePermission;
use linux_raw_sys::general::{AT_FDCWD, MFD_CLOEXEC, O_CLOEXEC, O_CREAT, O_EXCL, O_RDWR};

use crate::{
    file::{File, memfd},
    mm::UserConstPtr,
};

use super::fd_ops::openat_inner;

const MEMFD_NAME_MAX: usize = 249;
const MEMFD_DIR: &str = "/tmp/memfd";

static MEMFD_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let path = format!("{MEMFD_DIR}/.memfd-{id:016x}");
        match openat_inner(AT_FDCWD as _, &path, open_flags as i32, 0o600) {
            Ok(fd) => {
                let file = crate::file::get_typed_file::<File>(fd as i32)?;
                memfd::install_memfd_state(file.inner().location(), allow_sealing);
                return Ok(fd);
            }
            Err(AxError::AlreadyExists) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(AxError::AlreadyExists)
}
