use alloc::vec::Vec;
use core::{
    ffi::c_char,
    sync::atomic::{AtomicU64, Ordering},
};

use axerrno::{AxError, AxResult};
use axfs_ng_vfs::{FsPath, FsPathBuf, MetadataUpdate, NodePermission};
use linux_raw_sys::general::{AT_FDCWD, MFD_CLOEXEC, O_CLOEXEC, O_CREAT, O_EXCL, O_RDWR};
use tk_linux_mm::{
    F_SEAL_EXEC, F_SEAL_SEAL, MFD_NAME_MAX_LEN, MemfdPlan, inode_mode, sanitize_flags,
};

use super::fd_ops::openat_inner;
use crate::{
    file::{File, current_fd_table, get_file_description, memfd},
    mm::{UserMemoryCapability, map_usercopy_error, memfd_noexec_scope},
};

/// `mm/memfd.c:alloc_name()` copies at most `MFD_NAME_MAX_LEN + 1` bytes and
/// rejects a length above `MFD_NAME_MAX_LEN`, so the terminating NUL must
/// appear within this many bytes of the user pointer.
const MEMFD_NAME_SCAN_LEN: usize = MFD_NAME_MAX_LEN + 1;
const MEMFD_DIR: &FsPath = FsPath::new(b"/tmp/memfd");

static MEMFD_COUNTER: AtomicU64 = AtomicU64::new(0);

fn memfd_path(id: u64) -> AxResult<FsPathBuf> {
    let capacity = MEMFD_DIR
        .as_bytes()
        .len()
        .checked_add(1 + ".memfd-".len() + 16)
        .ok_or(AxError::NoMemory)?;
    let mut path = Vec::new();
    path.try_reserve_exact(capacity)
        .map_err(|_| AxError::NoMemory)?;
    path.extend_from_slice(MEMFD_DIR.as_bytes());
    path.extend_from_slice(b"/.memfd-");
    for shift in (0..16).rev().map(|n| n * 4) {
        let nibble = ((id >> shift) & 0xf) as u8;
        path.push(if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        });
    }
    Ok(FsPathBuf::from_vec(path))
}

/// `mm/memfd.c:alloc_name()`: scan for the NUL exactly like
/// `strncpy_from_user(&name[6], uname, MFD_NAME_MAX_LEN + 1)`, which returns
/// the length *without* the terminator and rejects anything above
/// `MFD_NAME_MAX_LEN`. An empty name is accepted, and any read fault —
/// including a NULL pointer — is `-EFAULT`.
fn validate_memfd_name(capability: &UserMemoryCapability, name: *const c_char) -> AxResult<()> {
    let start = name as usize;
    for offset in 0..MEMFD_NAME_SCAN_LEN {
        let address = start.checked_add(offset).ok_or(AxError::BadAddress)?;
        let byte = capability
            .read_value(address as *const u8)
            .map_err(map_usercopy_error)?;
        if byte == 0 {
            return Ok(());
        }
    }
    Err(AxError::InvalidInput)
}

fn ensure_memfd_dir() -> AxResult<()> {
    let fs_context = crate::task::current_fs_context();
    let fs = fs_context.lock();
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

/// Applies the mode Linux gives a `memfd_create(2)` inode.
///
/// `mm/shmem.c:__shmem_file_setup()` creates `S_IFREG | S_IRWXUGO` through an
/// inode setup that never consults the caller's umask, and
/// `mm/memfd.c:memfd_alloc_file()` then clears the execute bits for
/// `MFD_NOEXEC_SEAL`. The mode is user-visible through `fstat(2)`, and it is
/// the mechanism by which `MFD_NOEXEC_SEAL` denies `execve(2)`, so the umask
/// applied by the local create path has to be undone explicitly.
fn apply_memfd_inode_mode(location: &axfs_ng_vfs::Location, plan: MemfdPlan) -> AxResult<()> {
    location.update_metadata(MetadataUpdate {
        mode: Some(NodePermission::from_bits_truncate(inode_mode(plan))),
        ..Default::default()
    })?;
    Ok(())
}

pub fn sys_memfd_create(
    capability: UserMemoryCapability,
    name: *const c_char,
    flags: u32,
) -> AxResult<isize> {
    // Linux `SYSCALL_DEFINE2(memfd_create)` runs `sanitize_flags()` *before*
    // `alloc_name()`, so a bad flag word is reported even when the name
    // pointer is also bad. The reverse order made a bad pointer win.
    let plan = sanitize_flags(flags, memfd_noexec_scope() as u8).map_err(|error| match error {
        tk_linux_mm::MmError::InvalidMemfdFlags => AxError::InvalidInput,
        tk_linux_mm::MmError::MemfdNoexecEnforced => AxError::PermissionDenied,
        _ => AxError::InvalidInput,
    })?;
    validate_memfd_name(&capability, name)?;

    // This kernel has a real hugetlbfs type but no anonymous per-hstate
    // hugetlb file setup, so it matches a Linux built without
    // `CONFIG_HUGETLBFS`, whose `hugetlb_file_setup()` stub returns
    // `ERR_PTR(-ENOSYS)`.
    if plan.hugetlb {
        return Err(AxError::Unsupported);
    }

    ensure_memfd_dir()?;
    let mut open_flags = O_RDWR | O_CREAT | O_EXCL;
    if plan.flags & MFD_CLOEXEC != 0 {
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
                        let location = file.inner().location();
                        apply_memfd_inode_mode(location, plan)?;
                        memfd::install_memfd_state(
                            location,
                            if plan.may_seal {
                                if plan.noexec_seal { F_SEAL_EXEC } else { 0 }
                            } else {
                                F_SEAL_SEAL
                            },
                        )
                        .map(drop)
                    });
                if let Err(error) = install {
                    drop(description);
                    let removed = current_fd_table().close_if_same(fd as i32, expected);
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
