use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult};
use linux_raw_sys::general::{MFD_CLOEXEC, O_LARGEFILE, O_RDWR};
use tk_linux_mm::{MFD_NAME_MAX_LEN, sanitize_flags};

use crate::{
    file::{File, FileDescription, memfd_provider, reserve_fd},
    mm::{UserMemoryCapability, map_usercopy_error, memfd_noexec_scope},
    task::AsThread,
};

const MEMFD_NAME_SCAN_LEN: usize = MFD_NAME_MAX_LEN + 1;

fn load_memfd_name(capability: &UserMemoryCapability, name: *const c_char) -> AxResult<Vec<u8>> {
    let start = name as usize;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(MFD_NAME_MAX_LEN)
        .map_err(|_| AxError::NoMemory)?;
    for offset in 0..MEMFD_NAME_SCAN_LEN {
        let address = start.checked_add(offset).ok_or(AxError::BadAddress)?;
        let byte = capability
            .read_value(address as *const u8)
            .map_err(map_usercopy_error)?;
        if byte == 0 {
            return Ok(bytes);
        }
        if offset == MFD_NAME_MAX_LEN {
            return Err(AxError::InvalidInput);
        }
        bytes.push(byte);
    }
    Err(AxError::InvalidInput)
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
    let name = load_memfd_name(&capability, name)?;

    // This kernel has a real hugetlbfs type but no anonymous per-hstate
    // hugetlb file setup, so it matches a Linux built without
    // `CONFIG_HUGETLBFS`, whose `hugetlb_file_setup()` stub returns
    // `ERR_PTR(-ENOSYS)`.
    if plan.hugetlb {
        return Err(AxError::Unsupported);
    }

    // Anonymous shmem creation does not take a pathname write-open lease.
    // Linux alloc_file_pseudo() supplies a writable initial OFD without
    // FMODE_WRITER; reopening it for write still uses ordinary open admission.
    let reservation = reserve_fd(plan.flags & MFD_CLOEXEC != 0)?;
    let actor = axtask::current().as_thread().current_cred();
    let ids = actor.ids();
    let location = memfd_provider::create(&name, plan, ids.fsuid.into_raw(), ids.fsgid.into_raw())?;
    let file = Arc::try_new(File::new(axfs::File::new(
        axfs::FileBackend::Direct(location).with_direct_io(false),
        axfs::FileFlags::READ | axfs::FileFlags::WRITE,
    )))
    .map_err(|_| AxError::NoMemory)?;
    let description = FileDescription::new_with_flags(file, O_RDWR | O_LARGEFILE)?;
    Ok(reservation.prepare_publication(description)?.commit() as isize)
}
